use std::io;
use std::io::{Read, Write};
use std::fs;
use std::fs::File;
use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};
use mio::{Token, Ready, Poll, PollOpt};
use mio::net::TcpStream;
use rml_rtmp::handshake::{Handshake, PeerType, HandshakeProcessResult};
use rml_rtmp::chunk_io::{ChunkDeserializer, Packet};
use rml_rtmp::messages::RtmpMessage;

const BUFFER_SIZE: usize = 4096;

pub enum ReadResult {
    HandshakingInProgress,
    NoBytesReceived,
    BytesReceived {
        buffer: [u8; BUFFER_SIZE],
        byte_count: usize,
    },
}

#[derive(Debug)]
pub enum ConnectionError {
    IoError(io::Error),
    SocketClosed,
}

impl From<io::Error> for ConnectionError {
    fn from(error: io::Error) -> Self {
        ConnectionError::IoError(error)
    }
}

enum SendablePacket {
    RawBytes(Vec<u8>),
    Packet(Packet),
}

pub struct Connection {
    socket: TcpStream,
    pub token: Option<Token>,
    interest: Ready,
    send_queue: VecDeque<SendablePacket>,
    has_been_registered: bool,
    handshake: Handshake,
    handshake_completed: bool,
    debug_file: Option<File>,
    debug_deserializer: Option<ChunkDeserializer>,
    dropped_packet_count: u32,
    last_drop_notification_at: SystemTime,
}

impl Connection {
    pub fn new(socket: TcpStream, count: usize, log_debug_logic: bool) -> Connection {
        let (debug_file, deserializer) = match log_debug_logic {
            true => {
                fs::create_dir_all("logs").unwrap();

                let duration = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
                let seconds = duration.as_secs();
                let name = format!("logs/{}-{}.log", seconds, count);
                let file = File::create(name).unwrap();
                let deserializer = ChunkDeserializer::new();
                (Some(file), Some(deserializer))
            },

            false => (None, None),
        };

        Connection {
            socket,
            token: None,
            interest: Ready::readable() | Ready::writable(),
            send_queue: VecDeque::new(),
            has_been_registered: false,
            handshake: Handshake::new(PeerType::Server),
            handshake_completed: false,
            debug_deserializer: deserializer,
            debug_file,
            dropped_packet_count: 0,
            last_drop_notification_at: SystemTime::now(),
        }
    }

    pub fn enqueue_response(&mut self, poll: &mut Poll, bytes: Vec<u8>) -> io::Result<()> {
        self.send_queue.push_back(SendablePacket::RawBytes(bytes));
        self.interest.insert(Ready::writable());
        self.register(poll)
    }

    pub fn enqueue_packet(&mut self, poll: &mut Poll, packet: Packet) -> io::Result<()> {
        let elapsed = self.last_drop_notification_at.elapsed().unwrap();
        if elapsed.as_secs() > 10 {
            if self.dropped_packet_count > 0 {
                println!("{} packets dropped in the last {} seconds",
                         self.dropped_packet_count,
                         elapsed.as_secs());
            }

            self.last_drop_notification_at = SystemTime::now();
            self.dropped_packet_count = 0;
        }

        if packet.can_be_dropped && self.send_queue.len() > 10 {
            self.dropped_packet_count += 1;
            Ok(())
        } else {
            self.send_queue.push_back(SendablePacket::Packet(packet));
            self.interest.insert(Ready::writable());
            self.register(poll)
        }
    }

    pub fn readable(&mut self, poll: &mut Poll) -> Result<ReadResult, ConnectionError> {
        let mut buffer = [0_u8; 4096];
        match self.socket.read(&mut buffer) {
            Ok(0) => {
                Err(ConnectionError::SocketClosed)
            },

            Ok(bytes_read_count) => {
                let read_bytes = match self.handshake_completed {
                    false => self.handle_handshake_bytes(poll, &buffer[..bytes_read_count])?,
                    true => ReadResult::BytesReceived {buffer, byte_count: bytes_read_count},
                };

                self.register(poll)?;
                Ok(read_bytes)
            },

            Err(error) => {
                if error.kind() == io::ErrorKind::WouldBlock {
                    // There's no data available in the receive buffer, stop trying until the
                    // next readable event.
                    self.register(poll)?;
                    Ok(ReadResult::NoBytesReceived)
                } else {
                    println!("Failed to send buffer for {:?} with error {}", self.token, error);
                    return Err(ConnectionError::IoError(error));
                }
            }
        }
    }

    pub fn writable(&mut self, poll: &mut Poll) -> io::Result<()> {
        let message = match self.send_queue.pop_front() {
            Some(x) => x,
            None => {
                // Queue was empty, so we are no longer interested in writable events
                self.interest.remove(Ready::writable());
                self.register(poll)?;
                return Ok(());
            }
        };

        let bytes = match message {
            SendablePacket::RawBytes(bytes) => bytes,
            SendablePacket::Packet(packet) => packet.bytes,
        };

        match self.socket.write(&bytes) {
            Ok(_bytes_sent) => {
                if self.handshake_completed && self.debug_file.is_some() {
                    match self.debug_deserializer.as_mut().unwrap().get_next_message(&bytes).unwrap() {
                        Some(payload) => {
                            let inner_message = payload.to_rtmp_message().unwrap();
                            writeln!(self.debug_file.as_mut().unwrap(), "{:?}", payload).unwrap();

                            match inner_message {
                                RtmpMessage::VideoData {data} => {
                                    let output = format!("VideoData {{ data: [0x{:x}, 0x{:x}, 0x{:x}, 0x{:x}, 0x{:x}, ..] }}", data[0], data[1], data[2], data[3], data[4]);
                                    writeln!(self.debug_file.as_mut().unwrap(), "{}", output).unwrap();
                                },

                                RtmpMessage::AudioData {data} => {
                                    let output = format!("AudioData {{ data: [0x{:x}, 0x{:x}, ..] }}", data[0], data[1]);
                                    writeln!(self.debug_file.as_mut().unwrap(), "{}", output).unwrap();
                                },

                                RtmpMessage::SetChunkSize {size} => {
                                    writeln!(self.debug_file.as_mut().unwrap(), "{:?}", inner_message).unwrap();
                                    self.debug_deserializer
                                        .as_mut()
                                        .unwrap()
                                        .set_max_chunk_size(size as usize)
                                        .unwrap();
                                },

                                x => writeln!(self.debug_file.as_mut().unwrap(), "{:?}", x).unwrap(),
                            }
                        },
                        None => (),
                    }
                }
            },

            Err(error) => {
                if error.kind() == io::ErrorKind::WouldBlock {
                    // Client buffer is full, push it back to the queue
                    println!("Full write buffer!");
                    self.send_queue.push_front(SendablePacket::RawBytes(bytes));
                } else {
                    println!("Failed to send buffer for {:?} with error {}", self.token, error);
                    return Err(error);
                }
            }
        };

        if self.send_queue.is_empty() {
            self.interest.remove(Ready::writable());
        }

        self.register(poll)?;
        Ok(())
    }

    pub fn register(&mut self, poll: &mut Poll) -> io::Result<()> {
        match self.has_been_registered {
            true => poll.reregister(&self.socket, self.token.unwrap(), self.interest, PollOpt::edge() | PollOpt::oneshot())?,
            false => poll.register(&self.socket, self.token.unwrap(), self.interest, PollOpt::edge() | PollOpt::oneshot())?
        }

        self.has_been_registered = true;
        Ok(())
    }

    fn handle_handshake_bytes(&mut self, poll: &mut Poll, bytes: &[u8]) -> Result<ReadResult, ConnectionError> {
        let result = match self.handshake.process_bytes(bytes) {
            Ok(result) => result,
            Err(error) => {
                println!("Handshake error: {:?}", error);
                return Err(ConnectionError::SocketClosed);
            }
        };

        match result {
            HandshakeProcessResult::InProgress {response_bytes} => {
                if response_bytes.len() > 0 {
                    self.enqueue_response(poll, response_bytes)?;
                }

                Ok(ReadResult::HandshakingInProgress)
            },

            HandshakeProcessResult::Completed {response_bytes, remaining_bytes} => {
                println!("Handshake successful!");
                if response_bytes.len() > 0 {
                    self.enqueue_response(poll, response_bytes)?;
                }

                let mut buffer = [0; BUFFER_SIZE];
                let buffer_size = remaining_bytes.len();
                for (index, value) in remaining_bytes.into_iter().enumerate() {
                    buffer[index] = value;
                }

                self.handshake_completed = true;
                Ok(ReadResult::BytesReceived {buffer, byte_count: buffer_size})
            }
        }
    }
}