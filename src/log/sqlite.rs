//! The hash chain kept in SQLite: one `ops` row per sealed op. Payloads go
//! through a [`Codec`] (plain JSON, or the application's encryption, given
//! the op's hash to bind each payload to its row).

use std::path::Path;

use rusqlite::{params, Connection};
use serde::de::DeserializeOwned;
use serde::Serialize;

use super::chain::{seal_batch, Sealed, Tip};
use crate::{Error, Result};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS ops (
    seq    INTEGER PRIMARY KEY,
    id     TEXT NOT NULL UNIQUE,
    batch  TEXT NOT NULL,
    parent TEXT,
    ts     INTEGER NOT NULL,
    actor  TEXT NOT NULL,
    op     TEXT NOT NULL,
    hash   TEXT NOT NULL UNIQUE
);
CREATE INDEX IF NOT EXISTS ops_batch ON ops(batch);
";

/// How an op's payload is stored.
pub trait Codec<O> {
    fn encode(&self, op: &O, hash: &str) -> Result<String>;
    fn decode(&self, stored: &str, hash: &str) -> Result<O>;
}

/// Payloads as plain JSON.
pub struct PlainJson;

impl<O: Serialize + DeserializeOwned> Codec<O> for PlainJson {
    fn encode(&self, op: &O, _: &str) -> Result<String> {
        Ok(serde_json::to_string(op)?)
    }

    fn decode(&self, stored: &str, _: &str) -> Result<O> {
        Ok(serde_json::from_str(stored)?)
    }
}

/// A chain in SQLite. The tip is where the next op goes; verify the chain
/// (with [`super::chain::replay`]) before trusting it.
pub struct ChainLog {
    conn: Connection,
    tip: Tip,
}

impl ChainLog {
    /// Opens (or creates) the chain in a database file, in WAL mode.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::from_connection(conn)
    }

    /// Uses an open connection (which may hold the application's own tables too).
    pub fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA)?;
        let tip = conn
            .query_row("SELECT seq, hash FROM ops ORDER BY seq DESC LIMIT 1", [], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .map(|(seq, hash)| Tip { seq: seq as u64, head: Some(hash) })
            .or_else(|e| if e == rusqlite::Error::QueryReturnedNoRows { Ok(Tip::default()) } else { Err(e) })?;
        Ok(ChainLog { conn, tip })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    pub fn tip(&self) -> &Tip {
        &self.tip
    }

    /// Every op with `seq > after`, in order. A row that doesn't decode is an integrity error.
    pub fn read_since<A: DeserializeOwned, O>(&self, after: u64, codec: &impl Codec<O>) -> Result<Vec<Sealed<A, O>>> {
        let mut stmt = self.conn.prepare("SELECT seq, id, batch, parent, ts, actor, op, hash FROM ops WHERE seq > ?1 ORDER BY seq")?;
        let rows = stmt.query_map([after as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, id, batch, parent, ts, actor, op, hash) = row?;
            let seq = seq as u64;
            let bad = |reason: String| Error::Integrity { seq, reason };
            out.push(Sealed {
                seq,
                id: id.parse().map_err(|e| bad(format!("id: {e}")))?,
                batch: batch.parse().map_err(|e| bad(format!("batch: {e}")))?,
                parent,
                ts,
                actor: serde_json::from_str(&actor).map_err(|e| bad(format!("actor: {e}")))?,
                op: codec.decode(&op, &hash).map_err(|e| bad(format!("op: {e}")))?,
                hash,
            });
        }
        Ok(out)
    }

    /// Appends ops already sealed after the tip, in one transaction.
    pub fn append<A: Serialize, O>(&mut self, sealed: &[Sealed<A, O>], codec: &impl Codec<O>) -> Result<()> {
        let Some(first) = sealed.first() else { return Ok(()) };
        if first.seq != self.tip.seq + 1 || first.parent != self.tip.head {
            return Err(Error::Invalid("these ops don't follow the chain's tip".into()));
        }
        let payloads = sealed.iter().map(|o| codec.encode(&o.op, &o.hash)).collect::<Result<Vec<_>>>()?;
        let tx = self.conn.transaction()?;
        {
            let mut insert = tx.prepare("INSERT INTO ops (seq, id, batch, parent, ts, actor, op, hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)")?;
            for (o, payload) in sealed.iter().zip(&payloads) {
                insert.execute(params![o.seq as i64, o.id.to_string(), o.batch.to_string(), o.parent, o.ts, serde_json::to_string(&o.actor)?, payload, o.hash])?;
            }
        }
        tx.commit()?;
        let last = sealed.last().expect("non-empty");
        self.tip = Tip { seq: last.seq, head: Some(last.hash.clone()) };
        Ok(())
    }

    /// Seals `ops` as one batch after the tip and appends them.
    pub fn commit<A: Serialize + Clone, O: Serialize>(&mut self, actor: &A, ops: Vec<O>, ts: i64, codec: &impl Codec<O>) -> Result<Vec<Sealed<A, O>>> {
        if ops.is_empty() {
            return Err(Error::Invalid("empty batch".into()));
        }
        let sealed = seal_batch(self.tip.seq, self.tip.head.clone(), ts, actor, ops)?;
        self.append(&sealed, codec)?;
        Ok(sealed)
    }
}

#[cfg(test)]
mod tests {
    use super::super::chain::replay;
    use super::*;

    #[test]
    fn commits_reopens_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.db");
        {
            let mut log = ChainLog::open(&path).unwrap();
            log.commit(&"user".to_string(), vec![1i64, 2], 10, &PlainJson).unwrap();
            log.commit(&"user".to_string(), vec![3i64], 11, &PlainJson).unwrap();
            assert!(log.commit(&"user".to_string(), Vec::<i64>::new(), 12, &PlainJson).is_err());
        }
        let log = ChainLog::open(&path).unwrap();
        assert_eq!(log.tip().seq, 3);
        let ops: Vec<Sealed<String, i64>> = log.read_since(0, &PlainJson).unwrap();
        let mut sum = 0;
        let tip = replay(&ops, u64::MAX, |n| -> std::result::Result<(), String> {
            sum += n;
            Ok(())
        })
        .unwrap();
        assert_eq!((sum, &tip), (6, log.tip()));
        log.conn().execute("UPDATE ops SET op = '30' WHERE seq = 3", []).unwrap();
        let tampered: Vec<Sealed<String, i64>> = log.read_since(0, &PlainJson).unwrap();
        assert!(matches!(replay(&tampered, u64::MAX, |_| -> std::result::Result<(), String> { Ok(()) }), Err(Error::Integrity { seq: 3, .. })));
    }
}
