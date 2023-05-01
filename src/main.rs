use bluer::AdapterEvent;
use chrono::Timelike;
use futures::future::{pending, FusedFuture, FutureExt};
use futures::select;
use futures::stream::{FusedStream, Stream, StreamExt};
use std::pin::Pin;
use std::process::Command;
use tokio::time::{Duration, Instant};
use uuid::{uuid, Uuid};

const ARANET4_SERVICE: Uuid = uuid!("0000fce0-0000-1000-8000-00805f9b34fb");
const ARANET4_CHARACTERISTIC: Uuid = uuid!("f0cd3001-95da-4f4b-9ac8-aa55d312af0c");

#[derive(PartialEq, Debug)]
enum Status {
    GREEN = 1,
    AMBER = 2,
    RED = 3,
}

impl std::convert::TryFrom<u8> for Status {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            status if status == Status::GREEN as u8 => Ok(Status::GREEN),
            status if status == Status::AMBER as u8 => Ok(Status::AMBER),
            status if status == Status::RED as u8 => Ok(Status::RED),
            _ => Err(()),
        }
    }
}

#[derive(Debug)]
struct Data {
    co2: u16,
    _temperature: f32,
    _pressure: f32,
    _humidity: u8,
    _battery: u8,
    status: Status,
    interval: Duration,
    ago: Duration,
}

#[derive(Debug)]
struct DeviceInfo {
    _address: bluer::Address,
    data: Data,
}

#[derive(Debug)]
struct DeviceEvent {
    address: bluer::Address,
    next_update: Instant,
}

impl PartialEq for DeviceEvent {
    fn eq(&self, other: &Self) -> bool {
        self.next_update == other.next_update
    }
}

impl Eq for DeviceEvent {}

impl Ord for DeviceEvent {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.next_update.cmp(&self.next_update)
    }
}

impl PartialOrd for DeviceEvent {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn get_data(res: &[u8]) -> Data {
    Data {
        co2: u16::from_le_bytes(res[0..2].try_into().unwrap()),
        _temperature: u16::from_le_bytes(res[2..4].try_into().unwrap()) as f32 / 20.0,
        _pressure: u16::from_le_bytes(res[4..6].try_into().unwrap()) as f32 / 10.0,
        _humidity: u8::from_le(res[6]),
        _battery: u8::from_le(res[7]),
        status: u8::from_le(res[8]).try_into().unwrap(),
        interval: Duration::from_secs(u16::from_le_bytes(res[9..11].try_into().unwrap()) as u64),
        ago: Duration::from_secs(u16::from_le_bytes(res[11..13].try_into().unwrap()) as u64),
    }
}

async fn handle_device(
    adapter: &bluer::Adapter,
    address: &bluer::Address,
) -> Result<Option<DeviceInfo>, bluer::Error> {
    let device = adapter.device(*address)?;
    let service_uuids = device.uuids().await?.unwrap_or_default();
    if !service_uuids.contains(&ARANET4_SERVICE) {
        return Ok(None);
    }

    println!("Connect: {:?}", device.address());

    if !device.is_connected().await? {
        device.connect().await?;
    }

    for service in device.services().await? {
        if service.uuid().await? != ARANET4_SERVICE {
            continue;
        }

        for characteristic in service.characteristics().await? {
            if characteristic.uuid().await? != ARANET4_CHARACTERISTIC {
                continue;
            }

            let data = get_data(&characteristic.read().await?);

            device.disconnect().await?;
            return Ok(Some(DeviceInfo {
                _address: device.address(),
                data,
            }));
        }
    }

    device.disconnect().await?;
    Ok(None)
}

fn handle_event(
    event: &bluer::AdapterEvent,
    active_addresses: &mut std::collections::HashSet<bluer::Address>,
) -> Option<DeviceEvent> {
    match event {
        AdapterEvent::DeviceAdded(address) if !active_addresses.contains(address) => {
            active_addresses.insert(*address);
            Some(DeviceEvent {
                address: *address,
                next_update: Instant::now(),
            })
        }
        _ => None,
    }
}

struct StreamState {
    adapter: bluer::Adapter,
    event_stream: Pin<Box<dyn FusedStream<Item = bluer::AdapterEvent>>>,
    events_queue: std::collections::BinaryHeap<DeviceEvent>,
    active_addresses: std::collections::HashSet<bluer::Address>,
}

fn peek_next_timeout(
    events_queue: &mut std::collections::BinaryHeap<DeviceEvent>,
) -> Pin<Box<dyn FusedFuture<Output = ()>>> {
    match events_queue.peek() {
        Some(device_event) => {
            eprintln!(
                "Sleep ({}): {} -> {}s",
                &events_queue.len(),
                &device_event.address,
                (device_event.next_update - Instant::now()).as_secs_f32()
            );
            Box::pin(tokio::time::sleep_until(device_event.next_update).fuse())
        }
        None => Box::pin(pending().fuse()),
    }
}

async fn scan_devices() -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<DeviceInfo>>>>> {
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    let event_stream = adapter.discover_devices().await?.fuse();

    let stream_state = StreamState {
        adapter,
        event_stream: Box::pin(event_stream),
        events_queue: std::collections::BinaryHeap::new(),
        active_addresses: std::collections::HashSet::new(),
    };

    let device_info_stream = futures::stream::unfold(stream_state, |mut stream_state| async move {
        loop {
            // Go through the list and find the lowest next device we need to query.
            // Or wait for the next event.
            let mut timeout = peek_next_timeout(&mut stream_state.events_queue);

            select! {
                () = timeout => {
                    let device_event = stream_state.events_queue.pop().unwrap();
                    match handle_device(
                        &stream_state.adapter, &device_event.address).await {
                        // Relevant device and successfully retrieved infos.
                        // Reschedule after the relevant time.
                        Ok(Some(device_info)) => {
                            eprintln!("Rescheduling: {}", &device_event.address);
                            stream_state.events_queue.push(
                                DeviceEvent {
                                    address: device_event.address,
                                    next_update: Instant::now()
                                        + device_info.data.interval
                                        - device_info.data.ago
                                        + Duration::from_secs(5)
                                });
                            return Some((Ok(device_info), stream_state));
                        },
                        // Unrelated device: Remove it from the active set.
                        Ok(None) => {
                            eprintln!("Dropping: {}", device_event.address);
                            stream_state.active_addresses.remove(
                                &device_event.address);
                        },
                        Err(err) if err.kind == bluer::ErrorKind::NotFound => {
                            eprintln!("Not found: {}", device_event.address);
                            stream_state.active_addresses.remove(
                                &device_event.address);
                        },
                        // Reschedule again after a little while. But return the
                        // error to the consumer.
                        Err(err) => {
                            eprintln!("Try again on error {}: {}",
                                      &err,
                                      &device_event.address);
                            stream_state.events_queue.push(
                                DeviceEvent {
                                    address: device_event.address,
                                    next_update: Instant::now()
                                        + Duration::from_secs(60)});
                            return Some((Err(anyhow::Error::new(err)), stream_state));
                        }
                    }
                },
                maybe_event = stream_state.event_stream.next() => {
                    if let Some(event) = maybe_event {
                        if let Some(device_event) = handle_event(
                            &event, &mut stream_state.active_addresses) {
                            eprintln!("Scheduling check: {}", &device_event.address);
                            stream_state.events_queue.push(device_event);
                        }
                        ()
                    } else {
                        // Event stream has ended. Also end this stream.
                        return None;
                    }
                }
            };
        }
    });

    Ok(Box::pin(device_info_stream))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), anyhow::Error> {
    let mut last_trigger: Option<Instant> = None;
    let mut stream = scan_devices().await?;
    while let Some(stream_event) = stream.next().await {
        let device_info = match stream_event {
            Ok(device_info) => device_info,
            Err(err) => {
                eprintln!("stream error: {:?}", &err);
                continue;
            }
        };

        let local_time = chrono::Local::now();

        eprintln!(
            "We are {:?}: {}ppm CO2.",
            &device_info.data.status, &device_info.data.co2
        );

        if device_info.data.status != Status::GREEN {
            let is_too_recent = last_trigger
                .map(|last_trigger| Instant::now() - last_trigger < Duration::from_secs(10 * 60))
                .unwrap_or(false);
            // We did not trigger recently!
            if !is_too_recent
                // It's a sane time of the day!
                && local_time.hour() > 7
                && local_time.hour() < 22
            {
                eprintln!("Calling trigger.");
                Command::new("./trigger").output()?;
                last_trigger = Some(Instant::now());
            } else {
                eprintln!(
                    "Not calling trigger: is_too_recent={:?}, hour={:?}.",
                    is_too_recent,
                    local_time.hour()
                );
            }
        }
    }
    Ok(())
}
