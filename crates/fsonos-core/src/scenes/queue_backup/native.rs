//! Apply the durable backup limits to the shared native queue observation.

use super::{MAX_QUEUE_BACKUP_BYTES, MAX_QUEUE_BACKUP_ENTRIES};
use crate::queue_source::native::{self, Snapshot};
use fsonos_proto::{ProtoError, Transport};
use std::net::IpAddr;

pub(super) fn snapshot<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
) -> Result<Snapshot, ProtoError> {
    let limit = u32::try_from(MAX_QUEUE_BACKUP_ENTRIES).expect("the queue limit fits u32");
    native::snapshot(transport, host, limit, MAX_QUEUE_BACKUP_BYTES)
}
