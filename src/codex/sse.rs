use crate::{Error, Result};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use std::pin::Pin;

pub type ByteStream =
    Pin<Box<dyn Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

pub fn json_events(stream: ByteStream) -> impl Stream<Item = Result<Value>> + Send {
    async_stream::try_stream! {
        let mut stream = stream;
        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk?;
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| Error::upstream("upstream SSE was not UTF-8"))?;
            buffer.push_str(text);

            for event in drain_events(&mut buffer) {
                if event.data.trim().is_empty() || event.data.trim() == "[DONE]" {
                    continue;
                }
                yield serde_json::from_str::<Value>(&event.data)?;
            }
        }

        for event in drain_last_event(&mut buffer) {
            if !event.data.trim().is_empty() && event.data.trim() != "[DONE]" {
                yield serde_json::from_str::<Value>(&event.data)?;
            }
        }
    }
}

pub fn drain_events(buffer: &mut String) -> Vec<SseEvent> {
    let mut events = Vec::new();
    while let Some(index) = find_frame_end(buffer) {
        let frame = buffer[..index].to_owned();
        let next = if buffer[index..].starts_with("\r\n\r\n") {
            index + 4
        } else {
            index + 2
        };
        buffer.drain(..next);
        if let Some(event) = parse_frame(&frame) {
            events.push(event);
        }
    }
    events
}

fn drain_last_event(buffer: &mut String) -> Vec<SseEvent> {
    if buffer.trim().is_empty() {
        return Vec::new();
    }

    let frame = std::mem::take(buffer);
    parse_frame(&frame).into_iter().collect()
}

fn find_frame_end(buffer: &str) -> Option<usize> {
    match (buffer.find("\n\n"), buffer.find("\r\n\r\n")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn parse_frame(frame: &str) -> Option<SseEvent> {
    let mut event = None;
    let mut data = Vec::new();

    for line in frame.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start().to_owned());
        }
    }

    (!data.is_empty()).then(|| SseEvent {
        event,
        data: data.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drains_complete_frames_and_keeps_partial() {
        let mut buffer = "data: {\"a\":1}\n\n".to_owned();
        buffer.push_str("data: {\"b\":");

        let events = drain_events(&mut buffer);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"a\":1}");
        assert_eq!(buffer, "data: {\"b\":");
    }

    #[test]
    fn combines_multiline_data() {
        let mut buffer = "event: message\ndata: hello\ndata: world\n\n".to_owned();
        let events = drain_events(&mut buffer);

        assert_eq!(
            events,
            vec![SseEvent {
                event: Some("message".into()),
                data: "hello\nworld".into()
            }]
        );
    }
}
