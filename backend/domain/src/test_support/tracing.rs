//! Tracing capture utilities for focused backend unit tests.

use std::{
    collections::BTreeMap,
    future::Future,
    io,
    sync::{Arc, Mutex, Once},
};

use tracing::{Event, Id, Subscriber, field, span};
use tracing_subscriber::{
    Layer, fmt,
    layer::{Context, SubscriberExt},
    registry::LookupSpan,
    util::SubscriberInitExt,
};

/// A direct observation of one tracing lifecycle or event record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    SpanCreated {
        name: String,
        fields: BTreeMap<String, String>,
    },

    SpanRecorded {
        name: String,
        fields: BTreeMap<String, String>,
    },

    SpanClosed {
        name: String,
        fields: BTreeMap<String, String>,
    },

    Event {
        level: String,
        fields: BTreeMap<String, String>,
    },
}

#[derive(Default)]
struct RecordLayer {
    records: Arc<Mutex<Vec<Record>>>,
}

impl<S> Layer<S> for RecordLayer
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &Id, context: Context<'_, S>) {
        let Some(span) = context.span(id) else {
            return;
        };
        let mut fields = collect_attributes(attrs);
        for field in span.metadata().fields().iter() {
            fields
                .entry(field.name().to_owned())
                .or_insert_with(|| "<empty>".to_owned());
        }
        span.extensions_mut().insert(fields.clone());
        self.records
            .lock()
            .expect("capture records")
            .push(Record::SpanCreated {
                name: span.metadata().name().to_owned(),
                fields,
            });
    }

    fn on_record(&self, id: &Id, values: &span::Record<'_>, context: Context<'_, S>) {
        let Some(span) = context.span(id) else {
            return;
        };
        let fields = collect_record(values);
        if let Some(existing) = span.extensions_mut().get_mut::<BTreeMap<String, String>>() {
            existing.extend(fields.clone());
        }
        self.records
            .lock()
            .expect("capture records")
            .push(Record::SpanRecorded {
                name: span.metadata().name().to_owned(),
                fields,
            });
    }

    fn on_close(&self, id: Id, context: Context<'_, S>) {
        let Some(span) = context.span(&id) else {
            return;
        };
        let fields = span
            .extensions()
            .get::<BTreeMap<String, String>>()
            .cloned()
            .unwrap_or_default();
        self.records
            .lock()
            .expect("capture records")
            .push(Record::SpanClosed {
                name: span.metadata().name().to_owned(),
                fields,
            });
    }

    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let mut fields = BTreeMap::new();
        event.record(&mut FieldVisitor(&mut fields));
        self.records
            .lock()
            .expect("capture records")
            .push(Record::Event {
                level: event.metadata().level().to_string(),
                fields,
            });
    }
}

struct FieldVisitor<'a>(&'a mut BTreeMap<String, String>);

impl field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

#[derive(Clone)]
struct BufferWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl io::Write for BufferWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer
            .lock()
            .expect("capture buffer lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Captures records and formatted output while running a synchronous test action.
pub fn capture_records<F, T>(action: F) -> (T, Vec<Record>, String)
where
    F: FnOnce() -> T,
{
    initialize_test_subscriber();
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let records = Arc::new(Mutex::new(Vec::new()));
    let result = tracing::subscriber::with_default(
        test_subscriber(Arc::clone(&buffer), Arc::clone(&records)),
        action,
    );
    capture_result(result, buffer, records)
}

/// Captures records and formatted output while running an asynchronous test action.
pub async fn capture_records_async<F, Fut, T>(action: F) -> (T, Vec<Record>, String)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    initialize_test_subscriber();
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let records = Arc::new(Mutex::new(Vec::new()));
    let _guard = tracing::subscriber::set_default(test_subscriber(
        Arc::clone(&buffer),
        Arc::clone(&records),
    ));
    capture_result(action().await, buffer, records)
}

fn collect_attributes(values: &span::Attributes<'_>) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    values.record(&mut FieldVisitor(&mut fields));
    fields
}

fn collect_record(values: &span::Record<'_>) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    values.record(&mut FieldVisitor(&mut fields));
    fields
}

fn capture_result<T>(
    result: T,
    buffer: Arc<Mutex<Vec<u8>>>,
    records: Arc<Mutex<Vec<Record>>>,
) -> (T, Vec<Record>, String) {
    let output = String::from_utf8(buffer.lock().expect("capture buffer lock").clone())
        .expect("tracing output is UTF-8");
    (
        result,
        records.lock().expect("capture records lock").clone(),
        output,
    )
}

fn test_subscriber(
    buffer: Arc<Mutex<Vec<u8>>>,
    records: Arc<Mutex<Vec<Record>>>,
) -> impl Subscriber + Send + Sync {
    tracing_subscriber::registry()
        .with(RecordLayer { records })
        .with(
            fmt::layer()
                .with_ansi(false)
                .with_span_events(fmt::format::FmtSpan::CLOSE)
                .with_writer(move || BufferWriter {
                    buffer: Arc::clone(&buffer),
                })
                .with_filter(tracing_subscriber::EnvFilter::new("trace")),
        )
}

fn initialize_test_subscriber() {
    static TEST_SUBSCRIBER: Once = Once::new();
    TEST_SUBSCRIBER.call_once(|| {
        let _ = tracing_subscriber::registry().try_init();
    });
}
