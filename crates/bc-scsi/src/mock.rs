//! Scripted transport for testing drive logic without hardware.

use std::collections::HashMap;
use std::time::Duration;

use crate::{CommandResult, DataDir, DriveTransport, Result, STATUS_CHECK_CONDITION, STATUS_GOOD, Sense};

#[derive(Debug, Clone)]
pub enum Reply {
    Data(Vec<u8>),
    Check(Sense),
}

/// Answers each CDB by opcode, or by exact CDB when a more specific reply was registered.
#[derive(Default)]
pub struct MockTransport {
    by_cdb: HashMap<Vec<u8>, Reply>,
    by_opcode: HashMap<u8, Reply>,
    pub log: Vec<Vec<u8>>,
}

impl MockTransport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_opcode(mut self, opcode: u8, reply: Reply) -> Self {
        self.by_opcode.insert(opcode, reply);
        self
    }

    pub fn on_cdb(mut self, cdb: &[u8], reply: Reply) -> Self {
        self.by_cdb.insert(cdb.to_vec(), reply);
        self
    }
}

impl DriveTransport for MockTransport {
    fn execute(
        &mut self,
        cdb: &[u8],
        dir: DataDir,
        buf: &mut [u8],
        _timeout: Duration,
    ) -> Result<CommandResult> {
        self.log.push(cdb.to_vec());
        let reply = self
            .by_cdb
            .get(cdb)
            .or_else(|| self.by_opcode.get(&cdb[0]))
            .cloned()
            .unwrap_or(Reply::Check(Sense {
                key: crate::sense::key::ILLEGAL_REQUEST,
                asc: 0x20,
                ascq: 0,
            }));
        Ok(match reply {
            Reply::Data(d) => {
                let n = if dir == DataDir::In { d.len().min(buf.len()) } else { 0 };
                buf[..n].copy_from_slice(&d[..n]);
                CommandResult {
                    status: STATUS_GOOD,
                    sense: None,
                    transferred: n,
                }
            }
            Reply::Check(s) => CommandResult {
                status: STATUS_CHECK_CONDITION,
                sense: Some(s),
                transferred: 0,
            },
        })
    }
}
