use crate::engine::SseEvent;
use serde_json::Value;
use std::io;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Clone)]
pub struct RawOutput {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl RawOutput {
    pub fn stdout() -> Self {
        Self::new(io::stdout())
    }

    pub fn new<W>(writer: W) -> Self
    where
        W: Write + Send + 'static,
    {
        Self {
            writer: Arc::new(Mutex::new(Box::new(writer))),
        }
    }

    pub fn write_sse_event(&self, event: &SseEvent) -> io::Result<()> {
        if let Some(delta) = raw_model_delta_from_sse_event(event) {
            let mut writer = self
                .writer
                .lock()
                .map_err(|_| io::Error::other("raw output writer lock poisoned"))?;
            writer.write_all(delta.as_bytes())?;
            writer.flush()?;
        }
        Ok(())
    }
}

pub fn raw_model_delta_from_sse_event(event: &SseEvent) -> Option<&str> {
    event
        .event
        .ends_with(".delta")
        .then(|| event.data.get("delta").and_then(Value::as_str))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::RawOutput;
    use super::raw_model_delta_from_sse_event;
    use crate::engine::SseEvent;
    use serde_json::json;
    use std::io;
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::Mutex;

    #[test]
    fn extracts_any_sse_delta_with_a_string_delta_field() {
        assert_eq!(
            raw_model_delta_from_sse_event(&event("response.output_text.delta", "hel")),
            Some("hel")
        );
        assert_eq!(
            raw_model_delta_from_sse_event(&event(
                "response.reasoning_summary_text.delta",
                "think"
            )),
            Some("think")
        );
        assert_eq!(
            raw_model_delta_from_sse_event(&event("response.function_call_arguments.delta", "{}")),
            Some("{}")
        );
        assert_eq!(
            raw_model_delta_from_sse_event(&event("response.refusal.delta", "no")),
            Some("no")
        );
        assert_eq!(
            raw_model_delta_from_sse_event(&event("response.future_text.delta", "future")),
            Some("future")
        );
    }

    #[test]
    fn ignores_non_delta_events_and_delta_events_without_string_delta() {
        assert_eq!(
            raw_model_delta_from_sse_event(&event("response.output_text.done", "hello")),
            None
        );
        assert_eq!(
            raw_model_delta_from_sse_event(&SseEvent {
                event: "response.output_text.delta".to_string(),
                data: json!({ "delta": 1 }),
            }),
            None
        );
    }

    #[test]
    fn raw_output_writes_extracted_deltas_in_one_stream() {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let raw = RawOutput::new(SharedBuffer(Arc::clone(&buffer)));

        raw.write_sse_event(&event("response.output_text.delta", "hel"))
            .expect("output delta");
        raw.write_sse_event(&event("response.reasoning_summary_text.delta", "think"))
            .expect("reasoning delta");
        raw.write_sse_event(&event(
            "response.function_call_arguments.delta",
            "{\"x\":1}",
        ))
        .expect("function call delta");
        raw.write_sse_event(&event("response.refusal.delta", "no"))
            .expect("refusal delta");
        raw.write_sse_event(&event("response.output_text.done", "ignored"))
            .expect("ignored event");
        raw.write_sse_event(&event("response.output_text.delta", "lo"))
            .expect("second output delta");

        let output = buffer.lock().expect("buffer lock").clone();
        assert_eq!(
            String::from_utf8(output).expect("utf8"),
            "helthink{\"x\":1}nolo"
        );
    }

    fn event(name: &str, delta: &str) -> SseEvent {
        SseEvent {
            event: name.to_string(),
            data: json!({
                "type": name,
                "delta": delta,
            }),
        }
    }

    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| io::Error::other("buffer lock poisoned"))?
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
