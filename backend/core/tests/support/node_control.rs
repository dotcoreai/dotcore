//! A node control stream driven by hand, as a worker process drives it.
//!
//! Tests open streams on authenticated worker channels to act as a node incarnation the worker
//! process is not, then send node messages one at a time and read each manager answer, or the
//! status that ended the stream.

use dotcore_proto::dotcore::site::private::v1::{
    NodeControlOpen, NodeControlOpened, StreamNodeControlRequest, StreamNodeControlResponse,
    node_control_service_client::NodeControlServiceClient, stream_node_control_request,
    stream_node_control_response,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::{Status, Streaming, transport::Channel};

/// One open node control stream to the site manager.
pub struct ControlStream {
    requests: mpsc::UnboundedSender<StreamNodeControlRequest>,
    responses: Streaming<StreamNodeControlResponse>,
}

impl ControlStream {
    /// Opens a stream on `channel` whose first message is `open`.
    pub async fn open(channel: Channel, open: NodeControlOpen) -> Self {
        let (requests, outbound) = mpsc::unbounded_channel();
        requests
            .send(StreamNodeControlRequest {
                message: Some(stream_node_control_request::Message::Open(open)),
            })
            .expect("queued open");
        let responses = NodeControlServiceClient::new(channel)
            .stream_node_control(UnboundedReceiverStream::new(outbound))
            .await
            .expect("node control stream")
            .into_inner();
        Self {
            requests,
            responses,
        }
    }

    /// Sends one node message after the open.
    pub fn send(&self, message: stream_node_control_request::Message) {
        self.requests
            .send(StreamNodeControlRequest {
                message: Some(message),
            })
            .expect("queued node message");
    }

    /// Returns the next manager message, or the status that ended the stream.
    pub async fn next(&mut self) -> Result<stream_node_control_response::Message, Status> {
        self.responses
            .message()
            .await?
            .and_then(|response| response.message)
            .ok_or_else(|| Status::aborted("node control stream ended without a status"))
    }

    /// Returns the manager's answer to the open, failing on anything else.
    pub async fn opened(&mut self) -> NodeControlOpened {
        match self.next().await {
            Ok(stream_node_control_response::Message::Opened(opened)) => opened,
            other => panic!("expected the manager to answer the open, got {other:?}"),
        }
    }
}
