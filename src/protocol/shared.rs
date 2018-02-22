/*
 * Shared implementation details between server and client
 */
use bytes::{Bytes, BytesMut};
use errors::*;
use futures::{Future, Stream};
use futures::stream::SplitSink;
use protocol::protocol as proto;
use protocol::util::{
    self, Boxable, BoxFuture, HeartbeatAgent, SharedWriter,
    RcEventEmitter, EventSource, StreamThrottler, ThrottlingHandler};
use websocket::OwnedMessage;
use websocket::async::Client;
use websocket::stream::async::Stream as WsStream;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::rc::Rc;
use tokio_io::AsyncRead;
use protocol::codec::BytesCodec; // TODO: Switch back to tokio_io when upgraded to 0.1.5
use tokio_io::codec::Framed;
use tokio_core::net::TcpStream;
use tokio_core::reactor::Handle;

pub trait TwsServiceState<C: TwsConnection>: 'static + Sized {
    fn get_connections(&self) -> &HashMap<String, C>;

    /*
     * The `paused` state of a TwsService session
     * indicates whether the underlying WebSocket
     * connection has been congested or not.
     */
    fn set_paused(&mut self, paused: bool);
    fn get_paused(&self) -> bool;
}

/*
 * Throttle the TCP connection between
 *  1. server and remote
 *  2. local and client
 * based on the state of the WebSocket stream.
 * If the stream is congested, mark the corresponding
 * TCP connections as paused.
 */
pub struct TwsTcpReadThrottler<C: TwsConnection, T: TwsServiceState<C>> {
    _marker: PhantomData<C>,
    state: Rc<RefCell<T>>
}

impl<C, T> ThrottlingHandler for TwsTcpReadThrottler<C, T>
    where C: TwsConnection,
          T: TwsServiceState<C>
{
    fn pause(&self) {
        let mut state = self.state.borrow_mut();
        state.set_paused(true);
        for (_, v) in state.get_connections() {
            v.pause();
        }
    }

    fn resume(&self) {
        let mut state = self.state.borrow_mut();
        state.set_paused(false);
        for (_, v) in state.get_connections() {
            v.resume();
        }
    }

    fn is_paused(&self) -> bool {
        self.state.borrow().get_paused()
    }
}

/*
 * Shared logic abstracted from both the server and the client
 * should be implemented only on structs
 */
pub trait TwsService<C: TwsConnection, T: TwsServiceState<C>, S: 'static + WsStream>: 'static + Sized {
    /*
     * Required fields simulated by required methods.
     */
    fn get_passwd(&self) -> &str;
    fn get_writer(&self) -> &SharedWriter<SplitSink<Client<S>>>;
    fn get_heartbeat_agent(&self) -> &HeartbeatAgent<SplitSink<Client<S>>>;
    fn get_logger(&self) -> &util::Logger;
    fn get_state(&self) -> &Rc<RefCell<T>>;

    /*
     * Execute this service.
     * More precisely, execute the WebSocket-related services.
     * Receive from WebSocket and parse the TWS protocol packets.
     * This will consume the ownership of self. Please spawn
     * the returned future on an event loop.
     */
    fn run_service<'a>(self, client: Client<S>) -> BoxFuture<'a, ()> {
        let logger = self.get_logger().clone();
        let (sink, stream) = client.split();

        self.get_writer().set_throttling_handler(TwsTcpReadThrottler {
            _marker: PhantomData,
            state: self.get_state().clone()
        });

        // Obtain a future representing the writing tasks.
        let sink_write = self.get_writer().run(sink).map_err(clone!(logger; |e| {
            do_log!(logger, ERROR, "{:?}", e);
        }));

        // Obtain a future to do heartbeats.
        let heartbeat_work = self.get_heartbeat_agent().run().map_err(clone!(logger; |_| {
            do_log!(logger, ERROR, "Session timed out.");
        }));

        // The main task
        // 3 combined streams. Will finish once one of them finish.
        // i.e. when the connection closes, everything here should finish.
        stream
            .map_err(clone!(logger; |e| {
                do_log!(logger, ERROR, "{:?}", e);
                "session failed.".into()
            }))
            .for_each(move |msg| {
                // Process each message from the client
                // Note that this does not return a future.
                // Anything that happens for processing
                // the message and requires doing things
                // on the event loop should spawn a task
                // instead of returning it.
                // In order not to block the main WebSocket
                // stream.
                self.on_message(msg);
                Ok(()) as Result<()>
            })
            .select2(sink_write) // Execute task for the writer
            .select2(heartbeat_work) // Execute task for heartbeats
            .then(clone!(logger; |_| {
                do_log!(logger, INFO, "Session finished.");

                // Clean-up job
                // Drop all the connections
                // will be closed by the implementation of Drop
                //state.borrow_mut().remote_connections.clear();

                Ok(())
            }))
            ._box()
    }

    /*
     * Process new WebSocket packets
     */
    fn on_message(&self, msg: OwnedMessage) {
        match msg {
            // Control / Data packets can be in either Text or Binary form.
            OwnedMessage::Text(text) => self.on_packet(proto::parse_packet(&self.get_passwd(), text.as_bytes())),
            OwnedMessage::Binary(bytes) => self.on_packet(proto::parse_packet(&self.get_passwd(), &bytes)),

            // Send pong back to keep connection alive
            OwnedMessage::Ping(msg) => self.get_writer().feed(OwnedMessage::Pong(msg)),

            // Notify the heartbeat agent that a pong is received.
            OwnedMessage::Pong(_) => self.get_heartbeat_agent().set_heartbeat_received(),
            _ => ()
        };
    }

    /*
     * Process TWS protocol packets
     */
    fn on_packet(&self, packet: proto::Packet) {
        //do_log!(self.get_logger(), DEBUG, "{:?}", packet);
        match packet {
            // Call corresponding event methods.
            // Implementations can override these to control event handling.
            proto::Packet::Handshake(addr) => self.on_handshake(addr),
            proto::Packet::Connect(conn_id) => self.on_connect(conn_id),
            proto::Packet::ConnectionState((conn_id, state)) => self.on_connect_state(conn_id, state),
            proto::Packet::Data((conn_id, data)) => self.on_data(conn_id, data),

            // Process unknown packets
            _ => self.on_unknown()
        }
    }

    /*
     * Overridable events
     */
    fn on_unknown(&self) {}
    fn on_handshake(&self, _addr: SocketAddr) {}
    fn on_connect(&self, _conn_id: &str) {}
    fn on_connect_state(&self, _conn_id: &str, _ok: proto::ConnectionState) {}
    fn on_data(&self, _conn_id: &str, _data: &[u8]) {}
}

/*
 * Events and values that may be emitted by a TCP connection
 */
#[derive(PartialEq, Eq)]
pub enum ConnectionEvents {
    Data,
    Close
}

#[derive(Debug)]
pub enum ConnectionValues {
    Nothing,
    Packet(BytesMut)
}

// Splitted Sink for Tcp byte streams
pub type TcpSink = SplitSink<Framed<TcpStream, BytesCodec>>;

/*
 * Shared logic for TCP connection
 *  1. from server to remote
 *  2. from client to local (which is TWS client)
 */
pub trait TwsConnection: 'static + Sized + EventSource<ConnectionEvents, ConnectionValues> {
    fn get_endpoint_descriptors() -> (&'static str, &'static str) {
        ("remote", "client")
    }
    
    /*
     * Static method to bootstrap the connection
     * set up the event emitter and writer
     */
    fn create(
        conn_id: String, handle: Handle, logger: util::Logger, client: TcpStream
    ) -> (RcEventEmitter<ConnectionEvents, ConnectionValues>, SharedWriter<TcpSink>, StreamThrottler) {
        let (a, b) = Self::get_endpoint_descriptors();

        let emitter = util::new_emitter();
        let read_throttler = StreamThrottler::new();
        let (sink, stream) = client.framed(BytesCodec::new()).split();
        // SharedWriter for sending to remote
        let remote_writer = SharedWriter::new();

        // Forward remote packets to client
        let stream_work = read_throttler.wrap_stream(stream).for_each(clone!(a, emitter, logger, conn_id; |p| {
            do_log!(logger, INFO, "[{}] received {} bytes from {}", conn_id, p.len(), a);
            emitter.borrow().emit(ConnectionEvents::Data, ConnectionValues::Packet(p));
            Ok(())
        })).map_err(clone!(a, b, logger, conn_id; |e| {
            do_log!(logger, ERROR, "[{}] {} => {} error {:?}", conn_id, a, b, e);
        })).map(|_| ());

        // Forward client packets to remote
        // Client packets should be sent through `send` method.
        let sink_work = remote_writer.run(sink)
            .map_err(clone!(a, b, logger, conn_id; |e| {
                do_log!(logger, ERROR, "[{}] {} => {} error {:?}", conn_id, b, a, e);
            }));

        // Schedule the two jobs on the event loop
        // Use `select` to wait one of the jobs to finish.
        // This is often the `sink_work` if no error on remote side
        // has happened.
        // Once one of them is finished, just tear down the whole
        // channel.
        handle.spawn(stream_work.select(sink_work)
            .then(clone!(emitter, logger, conn_id; |_| {
                // Clean-up job upon finishing
                // No matter if there is any error.
                do_log!(logger, INFO, "[{}] Channel closing.", conn_id);
                emitter.borrow().emit(ConnectionEvents::Close, ConnectionValues::Nothing);
                Ok(())
            })));

        (emitter, remote_writer, read_throttler)
    }

    fn get_writer(&self) -> &SharedWriter<TcpSink>;
    fn get_conn_id(&self) -> &str;
    fn get_logger(&self) -> &util::Logger;
    fn get_read_throttler(&self) -> &StreamThrottler;
    fn get_read_pause_counter(&self) -> &Cell<usize>;

    /*
     * Send a data buffer to remote via the SharedWriter
     * created while connecting
     */
    fn send(&self, data: &[u8]) {
        // TODO: Do not feed if not connected
        do_log!(self.get_logger(), INFO, "[{}] sending {} bytes to {}", self.get_conn_id(), data.len(), Self::get_endpoint_descriptors().0);
        self.get_writer().feed(Bytes::from(data));
    }

    fn close(&self) {
        self.get_writer().close();
    }

    /*
     * Pause the reading part if it is not paused yet
     */
    fn pause(&self) {
        let counter = self.get_read_pause_counter();
        if counter.get() == 0 {
            self.get_read_throttler().pause();
        }
        counter.set(counter.get() + 1);
    }

    /*
     * Resume the reading part if no one requires it
     * to be paused
     */
    fn resume(&self) {
        let counter = self.get_read_pause_counter();
        if counter.get() == 1 {
            self.get_read_throttler().resume();
        }

        if counter.get() > 0 {
            counter.set(counter.get() - 1);
        }
    }
}