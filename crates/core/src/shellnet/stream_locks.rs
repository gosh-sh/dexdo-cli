//! Reconstruct active `PrivateNote` stream locks from successful inbound calls.

use std::collections::BTreeMap;

use anyhow::{anyhow, Result};
use base64::Engine as _;
use tvm_abi::token::TokenValue;
use tvm_abi::Contract;
use tvm_types::SliceData;

use super::contracts_provision::PRIVATENOTE_ABI;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoteStreamLockSnapshot {
    pub stream_count: u32,
    pub dispute_count: u32,
    pub last_change_unix: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NoteStreamLockKind {
    Stream,
    Dispute,
}

impl NoteStreamLockKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stream => "stream",
            Self::Dispute => "dispute",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteStreamLockEntry {
    pub deal: String,
    pub kind: NoteStreamLockKind,
    pub changed_at_unix: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NoteStreamLockCall {
    Lock {
        deal: String,
        kind: NoteStreamLockKind,
    },
    Unlock {
        deal: String,
        kind: NoteStreamLockKind,
    },
    ClearAll,
}

#[derive(Debug, Default)]
pub(super) struct NoteStreamLockFold {
    stream: BTreeMap<String, u64>,
    dispute: BTreeMap<String, u64>,
}

impl NoteStreamLockFold {
    pub(super) fn apply(&mut self, call: NoteStreamLockCall, changed_at_unix: u64) {
        match call {
            NoteStreamLockCall::Lock { deal, kind } => {
                self.locks_mut(kind).insert(deal, changed_at_unix);
            }
            NoteStreamLockCall::Unlock { deal, kind } => {
                self.locks_mut(kind).remove(&deal);
            }
            NoteStreamLockCall::ClearAll => {
                self.stream.clear();
                self.dispute.clear();
            }
        }
    }

    fn locks_mut(&mut self, kind: NoteStreamLockKind) -> &mut BTreeMap<String, u64> {
        match kind {
            NoteStreamLockKind::Stream => &mut self.stream,
            NoteStreamLockKind::Dispute => &mut self.dispute,
        }
    }

    pub(super) fn into_entries(self) -> Vec<NoteStreamLockEntry> {
        let mut entries =
            self.stream
                .into_iter()
                .map(|(deal, changed_at_unix)| NoteStreamLockEntry {
                    deal,
                    kind: NoteStreamLockKind::Stream,
                    changed_at_unix,
                })
                .chain(self.dispute.into_iter().map(|(deal, changed_at_unix)| {
                    NoteStreamLockEntry {
                        deal,
                        kind: NoteStreamLockKind::Dispute,
                        changed_at_unix,
                    }
                }))
                .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            (left.changed_at_unix, left.kind, &left.deal).cmp(&(
                right.changed_at_unix,
                right.kind,
                &right.deal,
            ))
        });
        entries
    }
}

pub(super) fn decode_note_stream_lock_call(
    body_b64: &str,
    internal: bool,
    internal_source: Option<&str>,
) -> Result<Option<NoteStreamLockCall>> {
    let bytes = match base64::engine::general_purpose::STANDARD.decode(body_b64.trim()) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let cell = match tvm_types::read_single_root_boc(&bytes) {
        Ok(cell) => cell,
        Err(_) => return Ok(None),
    };
    let slice = match SliceData::load_cell(cell) {
        Ok(slice) => slice,
        Err(_) => return Ok(None),
    };
    let contract = Contract::load(PRIVATENOTE_ABI.as_bytes())
        .map_err(|error| anyhow!("load PrivateNote ABI: {error}"))?;

    if !internal {
        let function = contract
            .function("forceClearStreamLocks")
            .map_err(|error| anyhow!("PrivateNote ABI has no forceClearStreamLocks: {error}"))?;
        return Ok(function
            .decode_input(slice, false, true)
            .is_ok()
            .then_some(NoteStreamLockCall::ClearAll));
    }

    for (method, kind, lock) in [
        ("streamLock", NoteStreamLockKind::Stream, true),
        ("streamUnlock", NoteStreamLockKind::Stream, false),
        ("streamDisputeLock", NoteStreamLockKind::Dispute, true),
        ("streamDisputeUnlock", NoteStreamLockKind::Dispute, false),
    ] {
        let function = contract
            .function(method)
            .map_err(|error| anyhow!("PrivateNote ABI has no {method}: {error}"))?;
        let Ok(tokens) = function.decode_input(slice.clone(), true, true) else {
            continue;
        };
        let deal = named_address(&tokens, "deal")
            .or_else(|| {
                internal_source
                    .filter(|source| !source.is_empty())
                    .map(str::to_string)
            })
            .ok_or_else(|| {
                anyhow!("PrivateNote lock call has no deal address or internal source")
            })?;
        return Ok(Some(if lock {
            NoteStreamLockCall::Lock { deal, kind }
        } else {
            NoteStreamLockCall::Unlock { deal, kind }
        }));
    }
    Ok(None)
}

fn named_address(tokens: &[tvm_abi::Token], name: &str) -> Option<String> {
    tokens
        .iter()
        .find_map(|token| match (&*token.name, &token.value) {
            (got, TokenValue::Address(value)) if got == name => Some(format!("{value}")),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock(deal: &str, kind: NoteStreamLockKind) -> NoteStreamLockCall {
        NoteStreamLockCall::Lock {
            deal: deal.to_string(),
            kind,
        }
    }

    fn unlock(deal: &str, kind: NoteStreamLockKind) -> NoteStreamLockCall {
        NoteStreamLockCall::Unlock {
            deal: deal.to_string(),
            kind,
        }
    }

    #[test]
    fn fold_lists_active_stream_and_dispute_deals() {
        let mut fold = NoteStreamLockFold::default();
        fold.apply(lock("0:stream-a", NoteStreamLockKind::Stream), 10);
        fold.apply(lock("0:dispute-a", NoteStreamLockKind::Dispute), 11);
        fold.apply(lock("0:stream-b", NoteStreamLockKind::Stream), 12);
        fold.apply(unlock("0:stream-a", NoteStreamLockKind::Stream), 13);

        let entries = fold.into_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].deal, "0:dispute-a");
        assert_eq!(entries[0].kind, NoteStreamLockKind::Dispute);
        assert_eq!(entries[1].deal, "0:stream-b");
        assert_eq!(entries[1].kind, NoteStreamLockKind::Stream);
    }

    #[test]
    fn decoded_force_clear_resets_reconstructed_deals() {
        let body = tvm_abi::encode_function_call(
            PRIVATENOTE_ABI,
            "forceClearStreamLocks",
            None,
            "{}",
            false,
            None,
            None,
        )
        .expect("encode forceClearStreamLocks body");
        let body = tvm_types::write_boc(&body.into_cell().expect("build call body"))
            .expect("serialize call body");
        let body = base64::engine::general_purpose::STANDARD.encode(body);
        let call = decode_note_stream_lock_call(&body, false, None)
            .expect("decode forceClearStreamLocks body")
            .expect("recognize forceClearStreamLocks body");
        assert_eq!(call, NoteStreamLockCall::ClearAll);

        let mut fold = NoteStreamLockFold::default();
        fold.apply(lock("0:old", NoteStreamLockKind::Stream), 10);
        fold.apply(call, 20);
        fold.apply(lock("0:new", NoteStreamLockKind::Stream), 30);

        let entries = fold.into_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].deal, "0:new");
    }
}
