use bevy::prelude::{App, Resource};
use bevy::tasks::AsyncComputeTaskPool;
use bevy::tasks::futures_lite::StreamExt;
use bevy::tasks::futures_lite::future::block_on;
use futures::SinkExt;
use futures::channel::mpsc;
use futures::channel::mpsc::{Sender, TryRecvError};
use r2r::geometry_msgs::msg::PoseStamped;
use r2r::rm_interfaces::msg::GimbalCmd;
use r2r::sensor_msgs::msg::{CameraInfo, CompressedImage, Image, JointState, PointCloud2};
use r2r::std_msgs::msg::String as RosString;
use r2r::tf2_msgs::msg::TFMessage;
use r2r::visualization_msgs::msg::Marker;
use r2r::{Node, QosProfile, WrappedTypesupport};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone)]
enum PublisherSender<T> {
    Buffered(Sender<T>),
    Latest {
        sender: crossbeam_channel::Sender<T>,
        receiver: crossbeam_channel::Receiver<T>,
    },
}

#[derive(Resource, Clone)]
pub struct TopicPublisher<T: RosTopic> {
    sender: PublisherSender<T::T>,
    last_publish: Arc<Mutex<Option<Instant>>>,
}

impl<T: RosTopic> TopicPublisher<T> {
    fn new(sender: PublisherSender<T::T>) -> Self {
        TopicPublisher {
            sender,
            last_publish: Arc::new(Mutex::new(None)),
        }
    }

    pub fn publish(&self, message: T::T) {
        self.publish_with(|| message);
    }

    pub fn publish_with(&self, create_message: impl FnOnce() -> T::T) {
        if T::MAX_PUBLISH_HZ > 0 {
            let interval = Duration::from_millis((1000 / T::MAX_PUBLISH_HZ as u64).max(1));
            let now = Instant::now();
            let mut last_publish = self.last_publish.lock().unwrap();
            if last_publish.is_some_and(|last| now.duration_since(last) < interval) {
                return;
            }
            *last_publish = Some(now);
        }

        let message = create_message();
        match &self.sender {
            PublisherSender::Buffered(sender) => {
                let mut sender = sender.clone();
                AsyncComputeTaskPool::get()
                    .spawn(async move {
                        let _ = sender.send(message).await;
                    })
                    .detach();
            }
            PublisherSender::Latest { sender, receiver } => {
                match sender.try_send(message) {
                    Ok(()) => {}
                    Err(crossbeam_channel::TrySendError::Full(message)) => {
                        // Replace the queued image with the freshest frame instead of
                        // allowing large RGB buffers to accumulate behind the network.
                        let _ = receiver.try_recv();
                        let _ = sender.try_send(message);
                    }
                    Err(crossbeam_channel::TrySendError::Disconnected(_)) => {}
                }
            }
        }
    }
}

#[derive(Resource)]
pub struct TopicSubscriber<T: RosTopic> {
    receiver: Arc<Mutex<Option<T::T>>>,
}

impl<T: RosTopic> TopicSubscriber<T> {
    pub(super) fn new() -> Self {
        TopicSubscriber {
            receiver: Arc::new(Mutex::new(None)),
        }
    }

    pub fn try_recv(&self) -> Result<Option<T::T>, TryRecvError> {
        Ok(self.receiver.lock().unwrap().take())
    }

    #[cfg(test)]
    pub(super) fn inject(&self, message: T::T) {
        *self.receiver.lock().unwrap() = Some(message);
    }
}

fn subscriber<T: RosTopic>(node: &mut Node, signal: Arc<AtomicBool>) -> TopicSubscriber<T> {
    let mut subscriber = node.subscribe::<T::T>(T::TOPIC, T::QOS).unwrap();
    let sub = TopicSubscriber::new();
    let mutex = sub.receiver.clone();
    std::thread::spawn(move || {
        while !signal.load(std::sync::atomic::Ordering::Acquire) {
            match block_on(subscriber.next()) {
                Some(msg) => {
                    mutex.lock().unwrap().replace(msg);
                }
                None => continue,
            }
        }
    });
    sub
}

fn publisher<T: RosTopic>(node: &mut Node, signal: Arc<AtomicBool>) -> TopicPublisher<T> {
    let publisher = node.create_publisher(T::TOPIC, T::QOS).unwrap();

    if T::MAX_PUBLISH_HZ > 0 {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        let worker_receiver = receiver.clone();
        std::thread::spawn(move || {
            while !signal.load(std::sync::atomic::Ordering::Acquire) {
                match worker_receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(message) => {
                        let _ = publisher.publish(&message);
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        TopicPublisher::new(PublisherSender::Latest { sender, receiver })
    } else {
        let (sender, mut receiver) = mpsc::channel(1024);
        AsyncComputeTaskPool::get()
            .spawn(async move {
                while !signal.load(std::sync::atomic::Ordering::Acquire) {
                    match receiver.next().await {
                        Some(message) => {
                            let _ = publisher.publish(&message);
                        }
                        None => break,
                    }
                }
            })
            .detach();
        TopicPublisher::new(PublisherSender::Buffered(sender))
    }
}

#[macro_export]
macro_rules! subscriber {
    ($signal:expr, $app:ident, $node:ident, $($topic:ty),* $(,)?) => {
        $(
            $app.insert_resource($crate::ros2::topic::subscriber::<$topic>($node, Arc::clone(&$signal)));
        )*
    };
}

pub trait RosTopic {
    type T: WrappedTypesupport + Send + 'static;
    const TOPIC: &'static str;
    const QOS: QosProfile;
    const MAX_PUBLISH_HZ: u32 = 0;
}

macro_rules! topic {
    ($topic:ident, $msg_typ:ty, $url:literal, max_rate_hz $rate:literal, $qos:expr) => {
        #[derive(Clone)]
        pub struct $topic;
        impl RosTopic for $topic {
            type T = $msg_typ;
            const TOPIC: &'static str = $url;
            const QOS: QosProfile = $qos;
            const MAX_PUBLISH_HZ: u32 = $rate;
        }
    };
    ($topic:ident, $msg_typ:ty, $url:literal, max_rate_hz $rate:literal) => {
        topic!($topic, $msg_typ, $url, max_rate_hz $rate, ::r2r::QosProfile::default());
    };
    ($topic:ident, $msg_typ:ty, $url:literal, $qos:expr) => {
        #[derive(Clone)]
        pub struct $topic;
        impl RosTopic for $topic {
            type T = $msg_typ;
            const TOPIC: &'static str = $url;
            const QOS: QosProfile = $qos;
        }
    };
    ($topic:ident, $msg_typ:ty, $url:literal) => {
        topic!($topic, $msg_typ, $url, ::r2r::QosProfile::default());
    };
    (pub {$($url:literal as $msg_typ:ty as $topic:ident $(max_rate_hz $rate:literal)? $(with $qos: expr)?;)*} $($remaining:tt)*) => {
        $(
            topic!($topic, $msg_typ, $url $(, max_rate_hz $rate)? $(, $qos)?);
        )*

        pub fn register_pub(atomic:Arc<AtomicBool>, app:&mut App, node:&mut Node) {
            $(
                app.insert_resource(publisher::<$topic>(node, atomic.clone()));
            )*
        }
        topic!($($remaining)*);
    };
    (sub {$($url:literal as $msg_typ:ty as $topic:ident $(with $qos: expr)?;)*} $($remaining:tt)*) => {
        $(
            topic!($topic, $msg_typ, $url $(, $qos)?);
        )*

        pub fn register_sub(atomic:Arc<AtomicBool>, app:&mut App, node:&mut Node) {
            $crate::subscriber!(atomic, app, node, $($topic,)*);
        }
        topic!($($remaining)*);
    };
    ( )=>{}
}

topic!(
    pub {
        "/camera_info" as CameraInfo as CameraInfoTopic;
        "/image_raw" as Image as ImageRawTopic max_rate_hz 20;
        "/image_compressed" as CompressedImage as ImageCompressedTopic max_rate_hz 20;
        "/livox/lidar" as PointCloud2 as LivoxPointCloudTopic;
        "/tf" as TFMessage as GlobalTransformTopic;
        "/simulator/marker" as Marker as OutpostMarkerTopic;
        "/gimbal_pose" as PoseStamped as GimbalPoseTopic;
        "/odom_pose" as PoseStamped as OdomPoseTopic;
        "/muzzle_pose" as PoseStamped as MuzzlePoseTopic;
        "/camera_pose" as PoseStamped as CameraPoseTopic;
        "/simulator/capture_pose" as PoseStamped as CapturePoseTopic;
        "/simulator/tech_core/state" as RosString as TechCoreStateTopic;
    }
    sub {
        "/rm_gimbal/cmd" as GimbalCmd as GimbalCmdTopic with QosProfile::sensor_data();
        "/simulator/gimbal_cmd" as GimbalCmd as SimGimbalCmdTopic with QosProfile::sensor_data();
        "/simulator/gimbal_setpoint" as JointState as SimGimbalSetpointTopic with QosProfile::sensor_data();
    }
);
