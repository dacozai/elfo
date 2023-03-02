use std::{collections::hash_map::Entry, hash::Hash, io, mem};

use fxhash::FxHashMap;
use serde::ser::SerializeStruct;
use tracing::Level;

use elfo_core::{
    dumping::{Dump, MessageKind, MessageName},
    node::{self, NodeNo},
};
use elfo_utils::ward;

use crate::{config::OnOverflow, rule_set::DumpParams};

// === Serializer ===

pub(crate) struct Serializer {
    class: &'static str,
    node_no: NodeNo,
    chunk_size: usize,
    /// A buffer to make complex names contiguous.
    name_buffer: String,
    /// A buffer for messages that serialized as strings.
    message_buffer: Vec<u8>,
    output: Vec<u8>,
    need_to_clear: bool,
    report: Report,
}

impl Serializer {
    pub(crate) fn new(class: &'static str) -> Self {
        Self::with_chunk_size(128 * 1024, class)
    }

    fn with_chunk_size(chunk_size: usize, class: &'static str) -> Self {
        // We should consider the limit and newlines, but the first one can be too
        // large and the number of newlines cannot be calculated before serialization.
        // So, just multiply the chunk's size by some coef, that's a good assumption.
        let initial_chunk_capacity = chunk_size * 3 / 2;

        Self {
            class,
            node_no: node::node_no(),
            chunk_size,
            name_buffer: String::new(),
            message_buffer: Vec::new(),
            output: Vec::with_capacity(initial_chunk_capacity),
            need_to_clear: false,
            report: Report::default(),
        }
    }

    pub(crate) fn append(&mut self, dump: &Dump, params: &DumpParams) -> Option<(&[u8], Report)> {
        self.clear_if_needed();

        #[cfg(debug_assertions)]
        let prev_len = self.output.len();

        match self.do_append(dump, params) {
            Ok(true) => {
                debug_assert_ne!(self.output.len(), prev_len);
                self.report.appended += 1;
                self.output.push(b'\n');
                self.take_if_limit_exceeded(self.chunk_size)
            }
            Ok(false) => {
                debug_assert_eq!(self.output.len(), prev_len);
                None
            }
            Err(err) => {
                debug_assert_eq!(self.output.len(), prev_len);
                self.report.add_failed(dump, err, params);
                None
            }
        }
    }

    /// * `Ok(true)` — appended.
    /// * `Ok(false)` — skipped.
    /// * `Err(err)` — failed.
    fn do_append(&mut self, dump: &Dump, params: &DumpParams) -> Result<bool, serde_json::Error> {
        let mut compact_dump = CompactDump {
            dump,
            class: self.class,
            node_no: self.node_no,
            message_name: dump.message_name.to_str(&mut self.name_buffer),
            message: None,
        };

        let prev_len = self.output.len();

        // Try to serialize directly into the output buffer.
        match serde_json::to_writer(
            LimitedWrite(&mut self.output, params.max_size),
            &compact_dump,
        ) {
            Ok(()) => return Ok(true),
            Err(err) => {
                // Either the limit is reached or the message is invalid.
                // Anyway, rollback the output buffer.
                self.output.truncate(prev_len);

                // `is_io()` returns true iff the limit is reached.
                if !err.is_io() {
                    return Err(err);
                }
            }
        }

        // If the limit is reached, we need to truncate the message or skip it.
        if params.on_overflow == OnOverflow::Skip {
            self.report.add_overflow(dump, false, params);
            return Ok(false);
        }

        self.message_buffer.clear();

        // Serialize the message into a temporary buffer with limitation.
        let _ = serde_json::to_writer(
            LimitedWrite(&mut self.message_buffer, params.max_size),
            &*dump.message,
        );

        // TODO: It should be done only on `err.is_io()`.
        //       However, `serde-json` returns `err.is_data()` here. Why?
        self.message_buffer.extend_from_slice(b" TRUNCATED");

        // Internally `serde-json` cannot write invalid UTF-8 if the limit is reached.
        // However, I don't want to rely on internal details even in rare cases.
        let message = String::from_utf8_lossy(&self.message_buffer);

        // Override the message and try to serialize into the output buffer again.
        compact_dump.message = Some(&message);

        serde_json::to_writer(&mut self.output, &compact_dump)
            .map(|_| {
                self.report.add_overflow(dump, true, params);
                true
            })
            .map_err(|err| {
                self.output.truncate(prev_len);
                err
            })
    }

    pub(crate) fn take(&mut self) -> Option<(&[u8], Report)> {
        self.clear_if_needed();
        self.take_if_limit_exceeded(0)
    }

    fn clear_if_needed(&mut self) {
        if self.need_to_clear {
            self.output.clear();
            self.need_to_clear = false;
        }
    }

    fn take_if_limit_exceeded(&mut self, limit: usize) -> Option<(&[u8], Report)> {
        if self.output.len() > limit {
            self.need_to_clear = true;
            Some((&self.output, mem::take(&mut self.report)))
        } else {
            None
        }
    }
}

// === Report ===

type MessageProtocol = &'static str;

#[derive(Debug, Default)]
pub(crate) struct Report {
    pub(crate) appended: usize,
    pub(crate) failed: FxHashMap<(MessageProtocol, MessageName), FailedDumpInfo>,
    pub(crate) overflow: FxHashMap<(MessageProtocol, MessageName, bool), OverflowDumpInfo>,
    // If new fields are added, update `Report::merge()`.
}

#[derive(Debug)]
pub(crate) struct FailedDumpInfo {
    pub(crate) level: Level,
    pub(crate) error: serde_json::Error,
    pub(crate) count: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OverflowDumpInfo {
    pub(crate) level: Level,
    pub(crate) count: usize,
}

impl Report {
    #[cold]
    fn add_failed(&mut self, dump: &Dump, error: serde_json::Error, params: &DumpParams) {
        let level = ward!(params.on_failure_log.into_level());

        self.failed
            .entry((dump.message_protocol, dump.message_name.clone()))
            .and_modify(|info| {
                info.level = level;
                info.count += 1;
            })
            .or_insert_with(|| FailedDumpInfo {
                level,
                error,
                count: 1,
            });
    }

    #[cold]
    fn add_overflow(&mut self, dump: &Dump, truncated: bool, params: &DumpParams) {
        let level = ward!(params.on_failure_log.into_level());

        self.overflow
            .entry((dump.message_protocol, dump.message_name.clone(), truncated))
            .and_modify(|info| {
                info.level = level;
                info.count += 1;
            })
            .or_insert_with(|| OverflowDumpInfo { level, count: 1 });
    }

    pub(crate) fn merge(&mut self, another: Report) {
        self.appended += another.appended;

        merge_maps(&mut self.failed, another.failed, |this, that| {
            this.level = that.level;
            this.count += that.count;
        });
        merge_maps(&mut self.overflow, another.overflow, |this, that| {
            this.level = that.level;
            this.count += that.count;
        });
    }
}

fn merge_maps<K: Eq + Hash, V>(
    dest: &mut FxHashMap<K, V>,
    src: FxHashMap<K, V>,
    f: impl Fn(&mut V, V),
) {
    for (key, that) in src {
        match dest.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(that);
            }
            Entry::Occupied(mut entry) => f(entry.get_mut(), that),
        }
    }
}

// === CompactDump ===

struct CompactDump<'a> {
    dump: &'a Dump,
    class: &'a str,
    node_no: NodeNo,
    message_name: &'a str,
    message: Option<&'a str>,
}

impl<'a> serde::Serialize for CompactDump<'a> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let field_count = 11
            + !self.dump.meta.key.is_empty() as usize // "k"
            + !matches!(self.dump.message_kind, MessageKind::Regular) as usize; // "c"

        let mut s = serializer.serialize_struct("Dump", field_count)?;

        // Dump `ts` firstly to make it possible to use `sort`.
        s.serialize_field("ts", &self.dump.timestamp)?;
        s.serialize_field("g", &self.dump.meta.group)?;

        if !self.dump.meta.key.is_empty() {
            s.serialize_field("k", &self.dump.meta.key)?;
        }

        s.serialize_field("n", &self.node_no)?;
        s.serialize_field("s", &self.dump.sequence_no)?;
        s.serialize_field("t", &self.dump.trace_id)?;
        s.serialize_field("th", &self.dump.thread_id)?;
        s.serialize_field("d", &self.dump.direction)?;
        s.serialize_field("cl", &self.class)?;
        s.serialize_field("mn", &self.message_name)?;
        s.serialize_field("mp", &self.dump.message_protocol)?;

        let (message_kind, correlation_id) = match self.dump.message_kind {
            MessageKind::Regular => ("Regular", None),
            MessageKind::Request(c) => ("Request", Some(c)),
            MessageKind::Response(c) => ("Response", Some(c)),
        };

        s.serialize_field("mk", message_kind)?;

        if let Some(message) = self.message {
            s.serialize_field("m", message)?;
        } else {
            s.serialize_field("m", &*self.dump.message)?;
        }

        if let Some(correlation_id) = correlation_id {
            s.serialize_field("c", &correlation_id)?;
        }

        s.end()
    }
}

// === LimitedWrite ===

struct LimitedWrite<W>(W, usize);

impl<W: io::Write> io::Write for LimitedWrite<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() > self.1 {
            return Ok(0);
        }

        self.1 -= buf.len();
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use elfo_core::{dumping::Timestamp, scope::Scope, tracing::TraceId, ActorMeta, Addr};

    use super::*;

    fn dump(sequence_no: u64, length: usize, is_good: bool) -> Dump {
        #[derive(serde::Serialize)]
        struct Some {
            body: String,
        }

        #[derive(serde::Serialize)]
        struct Bad(FxHashMap<(u32, u32), u32>);

        let scope = Scope::test(
            Addr::NULL,
            ActorMeta {
                group: "group".into(),
                key: "key".into(),
            }
            .into(),
        );
        scope.set_trace_id(TraceId::try_from(1).unwrap());
        let mut dump = scope.sync_within(|| {
            let mut builder = Dump::builder();
            builder.timestamp(Timestamp::from_nanos(2));
            builder.message_protocol("some");

            if is_good {
                builder.finish(Some {
                    body: "X".repeat(length),
                })
            } else {
                builder.finish(Bad(vec![((0, 1), 2)].into_iter().collect()))
            }
        });

        dump.sequence_no = sequence_no.try_into().unwrap();
        dump.thread_id = 0;
        dump
    }

    fn line(sequence_no: u64, length: usize) -> String {
        let template = r#"{"ts":2,"g":"group","k":"key","n":65535,"s":SEQNO,"t":1,"th":0,"d":"Out","cl":"some","mn":"Some","mp":"some","mk":"Regular","m":{"body":"BODY"}}"#;
        template
            .replace("SEQNO", &sequence_no.to_string())
            .replace("BODY", &"X".repeat(length))
    }

    #[test]
    fn normal() {
        let chunk_size = 1024;
        let mut serializer = Serializer::with_chunk_size(chunk_size, "some");

        let sample = dump(42, 4, true);
        let expected = line(42, 4);
        let mut expected_lines = chunk_size / (expected.len() + 1); // 1 for `\n`
        expected_lines += 1; // `append()` returns a chunk iff `chunk_size` is exceeded

        for _ in 0..5 {
            for _ in 1..expected_lines {
                assert!(serializer.append(&sample, &DumpParams::default()).is_none());
            }

            let (chunk, report) = serializer.append(&sample, &DumpParams::default()).unwrap();
            assert!(chunk.ends_with(b"\n"));
            assert_eq!(report.appended, expected_lines);
            assert!(report.failed.is_empty());
            assert!(report.overflow.is_empty());

            let chunk = std::str::from_utf8(chunk).unwrap();
            assert_eq!(chunk, format!("{expected}\n").repeat(expected_lines));
        }
    }

    #[test]
    fn skipped() {
        let chunk_size = 1024;
        let mut serializer = Serializer::with_chunk_size(chunk_size, "some");

        let sample = dump(42, 4, true);
        let expected = line(42, 4);
        let mut expected_lines = chunk_size / (expected.len() + 1); // 1 for `\n`
        expected_lines += 1; // `append()` returns a chunk iff `chunk_size` is exceeded

        for _ in 1..expected_lines {
            assert!(serializer.append(&sample, &DumpParams::default()).is_none());

            // Must be skipped, too restrictive.
            let params = DumpParams {
                max_size: 5,
                ..DumpParams::default()
            };
            assert!(serializer.append(&sample, &params).is_none());

            // Must be skipped, cannot be serialized.
            assert!(serializer
                .append(&dump(1, 1, false), &DumpParams::default())
                .is_none());
        }

        let (chunk, report) = serializer.append(&sample, &DumpParams::default()).unwrap();
        assert!(chunk.ends_with(b"\n"));
        assert_eq!(report.appended, expected_lines);
        assert_eq!(report.overflow.len(), 1);
        assert_eq!(
            report.overflow.values().next().unwrap(),
            &OverflowDumpInfo {
                level: Level::WARN,
                count: expected_lines - 1,
            }
        );
        assert_eq!(report.failed.len(), 1);
        let failed_info = report.failed.values().next().unwrap();
        assert_eq!(failed_info.level, Level::WARN);
        assert_eq!(failed_info.count, expected_lines - 1);
        assert!(failed_info
            .error
            .to_string()
            .contains("key must be a string"));

        let chunk = std::str::from_utf8(chunk).unwrap();
        assert_eq!(chunk, format!("{expected}\n").repeat(expected_lines));
    }

    #[test]
    fn truncated() {
        let chunk_size = 1024;
        let mut serializer = Serializer::with_chunk_size(chunk_size, "some");

        let sample = dump(42, 4, true);
        let expected = r#"{"ts":2,"g":"group","k":"key","n":65535,"s":42,"t":1,"th":0,"d":"Out","cl":"some","mn":"Some","mp":"some","mk":"Regular","m":"{\"body\":\" TRUNCATED"}"#;
        let mut expected_lines = chunk_size / (expected.len() + 1); // 1 for `\n`
        expected_lines += 1; // `append()` returns a chunk iff `chunk_size` is exceeded

        let params = DumpParams {
            max_size: 10,
            on_overflow: OnOverflow::Truncate,
            ..DumpParams::default()
        };

        for _ in 1..expected_lines {
            // Must be truncated, too restrictive.
            assert!(serializer.append(&sample, &params).is_none());
        }

        // Must be truncated, too restrictive.
        let (chunk, report) = serializer.append(&sample, &params).unwrap();
        assert!(chunk.ends_with(b"\n"));
        assert_eq!(report.appended, expected_lines);
        assert_eq!(report.overflow.len(), 1);
        assert_eq!(
            report.overflow.values().next().unwrap(),
            &OverflowDumpInfo {
                level: Level::WARN,
                count: expected_lines,
            }
        );
        assert_eq!(report.failed.len(), 0);

        let chunk = std::str::from_utf8(chunk).unwrap();
        assert_eq!(chunk, format!("{expected}\n").repeat(expected_lines));
    }

    #[test]
    fn take() {
        let chunk_size = 1024;
        let mut serializer = Serializer::with_chunk_size(chunk_size, "some");

        let sample = dump(42, 4, true);
        let expected = line(42, 4);
        let expected_lines = chunk_size / (expected.len() + 1); // 1 for `\n`

        for _ in 0..5 {
            for _ in 0..expected_lines {
                assert!(serializer.append(&sample, &DumpParams::default()).is_none());
            }

            let (chunk, report) = serializer.take().unwrap();
            assert!(chunk.ends_with(b"\n"));
            assert_eq!(report.appended, expected_lines);
            assert!(report.failed.is_empty());
            assert!(report.overflow.is_empty());

            let chunk = std::str::from_utf8(chunk).unwrap();
            assert_eq!(chunk, format!("{expected}\n").repeat(expected_lines));
        }
    }
}
