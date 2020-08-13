use self::super::{
	data::{House, Message},
	gateway::{
		EventInitState, EventTypingStart,
		Frame,
		OpCodeEvent, OpCodeHello, OpCodeLogin
	},
	http::{
		PathInfo,
		RequestInfo, RequestBodyInfo
	}
};
use async_tungstenite::{
	tokio::connect_async as websocket_async,
	tungstenite::{
		Message as WebsocketMessage,
		protocol::frame::CloseFrame
	}
};
use futures::{sink::SinkExt, stream::StreamExt};
use reqwest::Client as HTTPClient;
use serde_json::{from_str as from_json, to_string as to_json};
use std::{
	fmt::Debug,
	future::{Future, ready},
	pin::Pin,
	result::Result as STDResult,
	time::Duration
};
use tokio::{
	join, select,
	sync::{Notify, mpsc::{Receiver, Sender, channel, error::SendError}},
	time::timeout
};

type Result<T> = STDResult<T, Error>;

pub struct Client<'u, 't> {
	addresses: (&'u str, &'u str),
	token: &'t str,
	http_client: HTTPClient
}

impl<'u, 't> Client<'u, 't> {
	pub fn new(token: &'t str) -> Self {
		Self {
			addresses: ("api.hiven.io", "swarm-dev.hiven.io"),
			token: token,
			http_client: HTTPClient::new()
		}
	}

	pub async fn start_gateway<E>(&self, event_handler: E) -> Result<()>
			where E: EventHandler {
		let gate_keeper = GateKeeper {
			client: self,
			event_handler: event_handler
		};

		gate_keeper.start_gateway().await
	}

	pub async fn send_message<R>(&self, room: R, content: String)
			where R: Into<u64> {
		execute_request(&self.http_client, RequestInfo {
			token: self.token.to_owned(),
			path: PathInfo::MessageSend {
				channel_id: room.into()
			},
			body: RequestBodyInfo::MessageSend {
				content: content
			}
		}, self.addresses.0).await;
	}
}

async fn execute_request<'a>(client: &HTTPClient, request: RequestInfo,
		base_url: &'a str) {
	let path = format!("https://{}/v1{}", base_url, request.path.path());
	let http_request = client.request(request.body.method(), &path)
		.header("authorization", request.token);

	let http_request = if request.body.method() != "GET" {
		http_request.header("content-type", "application/json")
			.body(to_json(&request.body).unwrap()) // Remove unwrap().
	} else {http_request};
	
	// Remove unwrap()s.
	http_request.send().await.unwrap().error_for_status().unwrap();
}

pub struct GateKeeper<'c, 'u, 't, E>
		where E: EventHandler {
	pub client: &'c Client<'u, 't>,
	pub event_handler: E
}

impl<'c, 'u, 't, E> GateKeeper<'c, 'u, 't, E>
		where E: EventHandler{
	pub async fn start_gateway(&self) -> Result<()> {
		let (outgoing_send, outgoing_receive) = channel(5);
		let (incoming_send, incoming_receive) = channel(5);

		match join!(
			self.manage_gateway(incoming_send, outgoing_receive),
			self.listen_gateway(incoming_receive, outgoing_send)
		) {
			(Ok(()), Ok(())) => Ok(()),
			(Err(err), Ok(())) => Err(err),
			(Ok(()), Err(err)) => Err(err),
			(Err(_), Err(err)) => Err(err)
		}
	}

	async fn manage_gateway(&self, mut sender: Sender<Frame>,
			mut receiver: Receiver<Option<Frame>>) -> Result<()> {
		let url = format!("wss://{}/socket", self.client.addresses.1);
		let mut socket = websocket_async(url).await.unwrap().0; // Remove unwrap().

		loop {
			let incoming_frame = socket.next();
			let outgoing_frame = receiver.next();

			select! {
				// Remove second unwrap().
				// Consider removing first unwrap(). (Can tungstenite return a None
				// before SocketClose?)
				frame = incoming_frame => match frame.unwrap().unwrap() {
					// Remove unwrap()s.
					WebsocketMessage::Text(frame) => match from_json::<Frame>(&frame) {
						
						Ok(frame) => sender.send(frame).await.unwrap(),
						// Uncomment to show events that can't yet be parsed.
						// Err(err) => println!("{:?}: {}", err, frame),
						_ => ()
					},
					WebsocketMessage::Close(close_data) => return Err(Error::socket_close(close_data)),
					frame @ _ => return Err(Error::expectation_failed(
						"Text or Close frames only", frame))
				},
				// Remove unwrap()s.
				frame = outgoing_frame => socket.send(WebsocketMessage::Text(
					to_json(&frame.flatten().unwrap()).unwrap())).await.unwrap()
			}
		}
	}

	async fn listen_gateway(&self, mut receiver: Receiver<Frame>,
			mut sender: Sender<Option<Frame>>) -> Result<()> {
		let notify = Notify::new();

		let heart_beat = match receiver.next().await {
			// We got what we needed.
			Some(Frame::Hello(OpCodeHello {heart_beat})) => {
				let bag = (sender.clone(), Duration::from_millis(heart_beat.into()));

				// Set heart_beat to our heart beat future.
				async {
					let (mut sender, duration) = bag;

					loop {
						// Remove unwrap().
						if let Ok(()) = timeout(duration, notify.notified()).await
							{return Result::Ok(())}
						sender.send(Some(Frame::HeartBeat)).await.unwrap();
					}
				}
			},
			// Expectation failed.
			Some(frame) => return Err(Error::expectation_failed(
				"Frame::Hello(...)", frame)),
			// The channel died, exit gracefully.
			None => return Ok(())
		};

		let login_frame = Frame::Login(OpCodeLogin {
			token: self.client.token.to_owned()
		});
		sender.send(Some(login_frame)).await?;

		let listener = async {
			let result = loop {
				match receiver.next().await {
					Some(Frame::Event(event)) => match event {
						OpCodeEvent::InitState(data) =>
							self.event_handler.on_connect(&self.client, data).await,
						OpCodeEvent::HouseJoin(data) =>
							self.event_handler.on_house_join(&self.client, data).await,
						OpCodeEvent::TypingStart(data) =>
							self.event_handler.on_typing(&self.client, data).await,
						OpCodeEvent::MessageCreate(data) =>
							self.event_handler.on_message(&self.client, data).await
					},
					// The channel died, exit gracefully.
					None => break Result::Ok(()),
					_ => unimplemented!() // Remove unimplemented!().
				}
			};

			notify.notify();
			result
		};

		match join!(heart_beat, listener) {
			(Ok(()), Ok(())) => Ok(()),
			(Err(err), Ok(())) => Err(err),
			(Ok(()), Err(err)) => Err(err),
			(Err(_), Err(err)) => Err(err)
		}
	}
}

#[derive(Debug)]
pub enum Error {
	ExpectationFailed(&'static str, String),
	SocketClose(Option<CloseFrame<'static>>),
	InternalChannelError(String)
}

impl Error {
	pub fn expectation_failed<S>(expected: &'static str, got: S) -> Self
			where S: Debug {
		Self::ExpectationFailed(expected, format!("{:?}", got))
	}

	pub fn socket_close(close_data: Option<CloseFrame<'static>>) -> Self {
		Self::SocketClose(close_data)
	}

	pub fn send_error<T>(error: SendError<T>) -> Self
			where T: Debug {
		Self::InternalChannelError(format!("{:?}", error))
	}
}

impl<T> From<SendError<T>> for Error
		where T: Debug {
	fn from(error: SendError<T>) -> Self {
		Self::send_error(error)
	}
}

pub trait EventHandler {
	fn on_connect<'c>(&self, _client: &'c Client<'c, 'c>, _event: EventInitState) -> Pin<Box<dyn Future<Output = ()> + 'c>> {
		// NoOp
		Box::pin(ready(()))
	}

	fn on_house_join<'c>(&self, _client: &'c Client<'c, 'c>, _event: House) -> Pin<Box<dyn Future<Output = ()> + 'c>> {
		// NoOp
		Box::pin(ready(()))
	}

	fn on_typing<'c>(&self, _client: &'c Client<'c, 'c>, _event: EventTypingStart) -> Pin<Box<dyn Future<Output = ()> + 'c>> {
		// NoOp
		Box::pin(ready(()))
	}

	fn on_message<'c>(&self, _client: &'c Client<'c, 'c>, _event: Message) -> Pin<Box<dyn Future<Output = ()> + 'c>> {
		// NoOp
		Box::pin(ready(()))
	}
}
