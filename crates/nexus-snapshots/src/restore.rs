use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    new_identifier, validation, Result, SnapshotError, SnapshotFileState, SnapshotManifest,
    SnapshotStore, FILE_POLICY, MAX_TRANSACTION_RECORDS,
};

const TRANSACTION_SCHEMA_VERSION: u32 = 1;
const MAX_TRANSACTION_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestoreStatus {
    Prepared,
    Applying,
    Applied,
    Committing,
    Committed,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RestoreTicket {
    pub schema: u32,
    pub ticket_id: String,
    pub snapshot_id: String,
    pub snapshot_manifest_sha256: String,
    pub profile_name: String,
    pub created_unix_ms: u64,
    pub needs_materialization: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RestoreOutcome {
    pub ticket_id: String,
    pub snapshot_id: String,
    pub status: RestoreStatus,
    pub needs_materialization: bool,
    pub materialization_pending: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TransactionSummary {
    pub ticket: RestoreTicket,
    pub status: RestoreStatus,
    pub materialization_pending: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoverDecision {
    ResumeApply,
    Rollback,
    ResumeCommit,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OperationStatus {
    Pending,
    Applying,
    BackedUp,
    Applied,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedFile {
    present: bool,
    size: u64,
    sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreOperation {
    index: usize,
    status: OperationStatus,
    original: ExpectedFile,
    desired_state: SnapshotFileState,
    desired_sha256: Option<String>,
    applied_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransactionRecord {
    schema: u32,
    ticket: RestoreTicket,
    status: RestoreStatus,
    materialization_pending: bool,
    operations: Vec<RestoreOperation>,
}

struct DesiredFile {
    bytes: Option<Vec<u8>>,
    mode: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestoreFault {
    AfterNamespace(usize),
    AfterRecord(usize),
}

impl SnapshotStore {
    /// Fully validate a snapshot and all current targets, then persist a
    /// serializable ticket. This does not mutate DSH profile content.
    pub fn prepare_restore(&self, snapshot_id: &str) -> Result<RestoreTicket> {
        self.ensure_store_directories()?;
        self.prune_terminal_transactions()?;
        if let Some(active) = self.pending_transactions()?.first() {
            return Err(SnapshotError::InvalidState(format!(
                "restore {} is already active for profile {}",
                active.ticket.ticket_id,
                self.profile_name()
            )));
        }
        let manifest = self.detail(snapshot_id)?;
        let snapshot_directory = self.snapshot_directory(snapshot_id)?;
        let manifest_bytes = validation::read_regular_bounded(
            &snapshot_directory.join("manifest.json"),
            crate::MAX_MANIFEST_BYTES,
        )?
        .ok_or_else(|| SnapshotError::Integrity("snapshot manifest disappeared".to_owned()))?
        .0;
        let ticket = RestoreTicket {
            schema: TRANSACTION_SCHEMA_VERSION,
            ticket_id: new_identifier("restore"),
            snapshot_id: snapshot_id.to_owned(),
            snapshot_manifest_sha256: validation::sha256_hex(&manifest_bytes),
            profile_name: self.profile_name().to_owned(),
            created_unix_ms: crate::unix_millis()?,
            needs_materialization: false,
        };
        let mut operations = Vec::with_capacity(FILE_POLICY.len());
        let mut needs_materialization = false;
        for (index, (entry, policy)) in manifest.files.iter().zip(FILE_POLICY.iter()).enumerate() {
            let target = self.target_path(policy)?;
            let current = validation::read_regular_bounded(&target, policy.max_bytes)?;
            let original = match &current {
                Some((bytes, _)) => ExpectedFile {
                    present: true,
                    size: bytes.len() as u64,
                    sha256: Some(validation::sha256_hex(bytes)),
                },
                None => ExpectedFile {
                    present: false,
                    size: 0,
                    sha256: None,
                },
            };
            let desired = self.desired_file(
                &manifest,
                index,
                current.as_ref().map(|value| value.0.as_slice()),
            )?;
            let desired_sha256 = desired.bytes.as_deref().map(validation::sha256_hex);
            if index <= 2 && entry.state != SnapshotFileState::Omitted {
                let differs = match (&desired.bytes, &current) {
                    (Some(desired), Some((current, _))) => desired != current,
                    (None, None) => false,
                    _ => true,
                };
                needs_materialization |= differs;
            }
            operations.push(RestoreOperation {
                index,
                status: OperationStatus::Pending,
                original,
                desired_state: entry.state.clone(),
                desired_sha256,
                applied_sha256: None,
            });
        }
        let mut ticket = ticket;
        ticket.needs_materialization = needs_materialization;
        let record = TransactionRecord {
            schema: TRANSACTION_SCHEMA_VERSION,
            ticket: ticket.clone(),
            status: RestoreStatus::Prepared,
            materialization_pending: needs_materialization,
            operations,
        };
        self.write_transaction(&record)?;
        Ok(ticket)
    }

    /// Apply a prepared restore. Errors leave the durable transaction pending;
    /// callers choose explicit resume or rollback through [`recover_restore`].
    pub fn apply_restore(&self, ticket: &RestoreTicket) -> Result<RestoreOutcome> {
        self.apply_restore_inner(ticket, None, None)
    }

    /// Mark the caller-owned pnpm materialization step successful.
    pub fn mark_materialized(&self, ticket: &RestoreTicket) -> Result<RestoreOutcome> {
        let mut record = self.read_matching_transaction(ticket)?;
        match record.status {
            RestoreStatus::Applied => {}
            RestoreStatus::Committed if !record.materialization_pending => {
                return Ok(outcome(&record));
            }
            status => {
                return Err(SnapshotError::InvalidState(format!(
                    "cannot mark materialized while restore is {status:?}"
                )));
            }
        }
        record.materialization_pending = false;
        self.write_transaction(&record)?;
        Ok(outcome(&record))
    }

    /// Commit removes the in-home undo copies. The outer Nexus journal must
    /// call this only after its own Committed decision and materialization.
    pub fn commit_restore(&self, ticket: &RestoreTicket) -> Result<RestoreOutcome> {
        self.commit_restore_inner(ticket, None)
    }

    fn commit_restore_inner(
        &self,
        ticket: &RestoreTicket,
        fault: Option<RestoreFault>,
    ) -> Result<RestoreOutcome> {
        let mut record = self.read_matching_transaction(ticket)?;
        match record.status {
            RestoreStatus::Committed => {
                self.remove_backup_directory(&record.ticket.ticket_id)?;
                return Ok(outcome(&record));
            }
            RestoreStatus::Committing => {}
            RestoreStatus::Applied => {
                if record.materialization_pending {
                    return Err(SnapshotError::InvalidState(
                        "dependency materialization is still pending".to_owned(),
                    ));
                }
                record.status = RestoreStatus::Committing;
                self.write_transaction(&record)?;
            }
            status => {
                return Err(SnapshotError::InvalidState(format!(
                    "cannot commit restore while it is {status:?}"
                )));
            }
        }
        self.remove_backup_directory(&record.ticket.ticket_id)?;
        maybe_inject_restore_failure(fault, RestoreFault::AfterNamespace(0))?;
        record.status = RestoreStatus::Committed;
        self.write_transaction(&record)?;
        maybe_inject_restore_failure(fault, RestoreFault::AfterRecord(0))?;
        Ok(outcome(&record))
    }

    /// Explicitly rollback all applied files from in-home rename backups.
    pub fn rollback_restore(&self, ticket: &RestoreTicket) -> Result<RestoreOutcome> {
        self.rollback_restore_inner(ticket, None)
    }

    fn rollback_restore_inner(
        &self,
        ticket: &RestoreTicket,
        fault: Option<RestoreFault>,
    ) -> Result<RestoreOutcome> {
        let mut record = self.read_matching_transaction(ticket)?;
        match record.status {
            RestoreStatus::RolledBack => {
                self.remove_backup_directory(&record.ticket.ticket_id)?;
                return Ok(outcome(&record));
            }
            RestoreStatus::Committed | RestoreStatus::Committing => {
                return Err(SnapshotError::InvalidState(
                    "committed restore no longer has rollback data".to_owned(),
                ));
            }
            RestoreStatus::Prepared => {
                record.status = RestoreStatus::RolledBack;
                record.materialization_pending = false;
                self.write_transaction(&record)?;
                return Ok(outcome(&record));
            }
            RestoreStatus::Applying | RestoreStatus::Applied => {}
        }
        let backup_directory = self.backup_directory(&record.ticket.ticket_id)?;
        let _ = validation::validate_optional_directory_tree(self.dsh_home(), &backup_directory)?;
        for operation_index in (0..record.operations.len()).rev() {
            let operation = record.operations[operation_index].clone();
            if matches!(
                operation.status,
                OperationStatus::Pending | OperationStatus::RolledBack
            ) {
                continue;
            }
            let policy = &FILE_POLICY[operation.index];
            let target = self.target_path(policy)?;
            let backup = self.backup_file(&record.ticket.ticket_id, operation.index)?;
            let backup_directory_present =
                validation::validate_optional_directory_tree(self.dsh_home(), &backup_directory)?;
            let backup_bytes = if backup_directory_present {
                validation::read_regular_bounded(&backup, policy.max_bytes)?
            } else {
                None
            };
            if let Some((backup_bytes, _)) = backup_bytes {
                if !matches_expected(Some(&backup_bytes), &operation.original) {
                    return Err(SnapshotError::Integrity(format!(
                        "transaction backup changed for {}",
                        policy.manifest_path
                    )));
                }
                let parent = target.parent().ok_or_else(|| {
                    SnapshotError::UnsafePath(format!("target has no parent: {}", target.display()))
                })?;
                validation::ensure_directory_tree(self.dsh_home(), parent)?;
                validation::validate_directory_tree(self.dsh_home(), &backup_directory)?;
                validation::replace_durable(&backup, &target)?;
            } else if operation.original.present {
                let current = validation::read_regular_bounded(&target, policy.max_bytes)?;
                if !matches_expected(
                    current.as_ref().map(|value| value.0.as_slice()),
                    &operation.original,
                ) {
                    return Err(SnapshotError::Integrity(format!(
                        "original file and backup are both unavailable for {}",
                        policy.manifest_path
                    )));
                }
            } else {
                self.move_rollback_discard(
                    &record.ticket.ticket_id,
                    operation.index,
                    &target,
                    policy.max_bytes,
                    &backup_directory,
                    if operation.status == OperationStatus::BackedUp {
                        operation.desired_sha256.as_deref()
                    } else { operation.applied_sha256.as_deref() },
                    operation.status == OperationStatus::Applied,
                )?;
            }
            maybe_inject_restore_failure(fault, RestoreFault::AfterNamespace(operation.index))?;
            record.operations[operation_index].status = OperationStatus::RolledBack;
            record.operations[operation_index].applied_sha256 = None;
            self.write_transaction(&record)?;
            maybe_inject_restore_failure(fault, RestoreFault::AfterRecord(operation.index))?;
        }
        self.remove_backup_directory(&record.ticket.ticket_id)?;
        record.status = RestoreStatus::RolledBack;
        record.materialization_pending = false;
        self.write_transaction(&record)?;
        Ok(outcome(&record))
    }

    /// Execute a caller-selected recovery decision. Store construction never
    /// invokes this method automatically.
    pub fn recover_restore(
        &self,
        ticket: &RestoreTicket,
        decision: RecoverDecision,
    ) -> Result<RestoreOutcome> {
        match decision {
            RecoverDecision::ResumeApply => self.apply_restore(ticket),
            RecoverDecision::Rollback => self.rollback_restore(ticket),
            RecoverDecision::ResumeCommit => self.commit_restore(ticket),
        }
    }

    pub fn pending_restores(&self) -> Result<Vec<TransactionSummary>> {
        self.pending_transactions()
    }

    fn apply_restore_inner(
        &self,
        ticket: &RestoreTicket,
        fail_after_operations: Option<usize>,
        namespace_fault: Option<RestoreFault>,
    ) -> Result<RestoreOutcome> {
        let mut record = self.read_matching_transaction(ticket)?;
        match record.status {
            RestoreStatus::Applied => return Ok(outcome(&record)),
            RestoreStatus::Prepared | RestoreStatus::Applying => {}
            status => {
                return Err(SnapshotError::InvalidState(format!(
                    "cannot apply restore while it is {status:?}"
                )));
            }
        }
        let manifest = self.revalidate_ticket_snapshot(&record.ticket)?;
        if record.status == RestoreStatus::Prepared {
            self.preflight_originals(&record)?;
            // Only a new apply is budget-gated. Resume and rollback of an
            // already-started transaction must remain possible on a full disk.
            let mut targets = Vec::new();
            for (index, policy) in FILE_POLICY.iter().enumerate() {
                let target = self.target_path(policy)?;
                let current = validation::read_regular_bounded(&target, policy.max_bytes)?;
                let desired = self.desired_file(&manifest, index, current.as_ref().map(|entry| entry.0.as_slice()))?;
                if let Some(bytes) = desired.bytes { targets.push((target, bytes.len() as u64 + 65536)); }
            }
            // Existing originals become in-home rename backups; only the new
            // contents and an atomic journal replacement need extra allocation.
            targets.push((self.transaction_root().to_path_buf(), 2 * MAX_TRANSACTION_BYTES));
            let budget: Vec<_> = targets.iter().map(|(path, bytes)| (path.as_path(), *bytes)).collect();
            nexus_private_file::ensure_space_budget(&budget)
                .map_err(|error| SnapshotError::io("restore target-volume space", error))?;
            record.status = RestoreStatus::Applying;
            self.write_transaction(&record)?;
        }
        let backup_directory = self.backup_directory(&record.ticket.ticket_id)?;
        validation::ensure_directory_tree(self.dsh_home(), &backup_directory)?;
        let mut completed = record
            .operations
            .iter()
            .filter(|operation| operation.status == OperationStatus::Applied)
            .count();
        for operation_index in 0..record.operations.len() {
            if record.operations[operation_index].status == OperationStatus::Applied {
                self.verify_applied_operation(&record.operations[operation_index])?;
                continue;
            }
            if record.operations[operation_index].desired_state == SnapshotFileState::Omitted {
                record.operations[operation_index].status = OperationStatus::Applied;
                self.write_transaction(&record)?;
                completed += 1;
                maybe_inject_failure(fail_after_operations, completed)?;
                continue;
            }
            let index = record.operations[operation_index].index;
            let policy = &FILE_POLICY[index];
            let target = self.target_path(policy)?;
            let backup = self.backup_file(&record.ticket.ticket_id, index)?;
            validation::validate_directory_tree(self.dsh_home(), &backup_directory)?;
            if record.operations[operation_index].status == OperationStatus::Pending {
                let current = validation::read_regular_bounded(&target, policy.max_bytes)?;
                if !matches_expected(
                    current.as_ref().map(|value| value.0.as_slice()),
                    &record.operations[operation_index].original,
                ) {
                    return Err(SnapshotError::ConcurrentModification(
                        policy.manifest_path.to_owned(),
                    ));
                }
                record.operations[operation_index].status = OperationStatus::Applying;
                self.write_transaction(&record)?;
            }
            if record.operations[operation_index].status == OperationStatus::Applying {
                if record.operations[operation_index].original.present {
                    let existing_backup =
                        validation::read_regular_bounded(&backup, policy.max_bytes)?;
                    if existing_backup.is_none() {
                        let current = validation::read_regular_bounded(&target, policy.max_bytes)?;
                        if !matches_expected(
                            current.as_ref().map(|value| value.0.as_slice()),
                            &record.operations[operation_index].original,
                        ) {
                            return Err(SnapshotError::ConcurrentModification(
                                policy.manifest_path.to_owned(),
                            ));
                        }
                        validation::validate_directory_tree(self.dsh_home(), &backup_directory)?;
                        validation::rename_durable(&target, &backup)?;
                    }
                    validation::validate_directory_tree(self.dsh_home(), &backup_directory)?;
                    let backup_bytes = validation::read_regular_bounded(&backup, policy.max_bytes)?
                        .ok_or_else(|| {
                            SnapshotError::Integrity(format!(
                                "missing transaction backup for {}",
                                policy.manifest_path
                            ))
                        })?;
                    if !matches_expected(
                        Some(&backup_bytes.0),
                        &record.operations[operation_index].original,
                    ) {
                        return Err(SnapshotError::Integrity(format!(
                            "transaction backup changed for {}",
                            policy.manifest_path
                        )));
                    }
                } else if validation::read_regular_bounded(&backup, policy.max_bytes)?.is_some() {
                    return Err(SnapshotError::Integrity(format!(
                        "unexpected backup for originally missing {}",
                        policy.manifest_path
                    )));
                }
                record.operations[operation_index].status = OperationStatus::BackedUp;
                self.write_transaction(&record)?;
            }
            let current = if record.operations[operation_index].original.present {
                validation::validate_directory_tree(self.dsh_home(), &backup_directory)?;
                Some(
                    validation::read_regular_bounded(&backup, policy.max_bytes)?
                        .ok_or_else(|| {
                            SnapshotError::Integrity("transaction backup disappeared".to_owned())
                        })?
                        .0,
                )
            } else {
                None
            };
            let desired = self.desired_file(&manifest, index, current.as_deref())?;
            let computed_desired_sha256 = desired.bytes.as_deref().map(validation::sha256_hex);
            if computed_desired_sha256 != record.operations[operation_index].desired_sha256 {
                return Err(SnapshotError::Integrity(format!(
                    "prepared desired content changed for {}",
                    policy.manifest_path
                )));
            }
            match desired.bytes {
                Some(bytes) => {
                    match validation::read_regular_bounded(&target, policy.max_bytes)? {
                        Some((existing, _)) if existing == bytes => {}
                        Some(_) => {
                            return Err(SnapshotError::ConcurrentModification(
                                policy.manifest_path.to_owned(),
                            ));
                        }
                        None => {
                            let parent = target.parent().ok_or_else(|| {
                                SnapshotError::UnsafePath(format!(
                                    "target has no parent: {}",
                                    target.display()
                                ))
                            })?;
                            validation::ensure_directory_tree(self.dsh_home(), parent)?;
                            validation::write_durable(&target, &bytes)?;
                            validation::apply_mode(&target, desired.mode)?;
                        }
                    }
                    record.operations[operation_index].applied_sha256 =
                        Some(validation::sha256_hex(&bytes));
                }
                None => {
                    if target.exists() {
                        return Err(SnapshotError::ConcurrentModification(
                            policy.manifest_path.to_owned(),
                        ));
                    }
                    record.operations[operation_index].applied_sha256 = None;
                }
            }
            maybe_inject_restore_failure(namespace_fault, RestoreFault::AfterNamespace(index))?;
            record.operations[operation_index].status = OperationStatus::Applied;
            self.write_transaction(&record)?;
            completed += 1;
            maybe_inject_failure(fail_after_operations, completed)?;
        }
        record.status = RestoreStatus::Applied;
        self.write_transaction(&record)?;
        Ok(outcome(&record))
    }

    fn desired_file(
        &self,
        manifest: &SnapshotManifest,
        index: usize,
        current: Option<&[u8]>,
    ) -> Result<DesiredFile> {
        let entry = manifest.files.get(index).ok_or_else(|| {
            SnapshotError::InvalidManifest(format!("missing file record {index}"))
        })?;
        let policy = &FILE_POLICY[index];
        match entry.state {
            SnapshotFileState::Present => {
                let snapshot = validation::read_snapshot_blob(self, &manifest.snapshot_id, index)?;
                let bytes = validation::merge_current_secrets(
                    policy,
                    &snapshot,
                    &entry.redacted_paths,
                    current,
                )?;
                if bytes.len() as u64 > policy.max_bytes {
                    return Err(SnapshotError::Oversized {
                        path: entry.path.clone(),
                        size: bytes.len() as u64,
                        limit: policy.max_bytes,
                    });
                }
                Ok(DesiredFile {
                    bytes: Some(bytes),
                    mode: entry.mode,
                })
            }
            SnapshotFileState::Missing | SnapshotFileState::Omitted => Ok(DesiredFile {
                bytes: None,
                mode: None,
            }),
        }
    }

    fn revalidate_ticket_snapshot(&self, ticket: &RestoreTicket) -> Result<SnapshotManifest> {
        let manifest = self.detail(&ticket.snapshot_id)?;
        let directory = self.snapshot_directory(&ticket.snapshot_id)?;
        let manifest_bytes = validation::read_regular_bounded(
            &directory.join("manifest.json"),
            crate::MAX_MANIFEST_BYTES,
        )?
        .ok_or_else(|| SnapshotError::Integrity("snapshot manifest disappeared".to_owned()))?
        .0;
        if validation::sha256_hex(&manifest_bytes) != ticket.snapshot_manifest_sha256 {
            return Err(SnapshotError::Integrity(
                "snapshot manifest changed after prepare".to_owned(),
            ));
        }
        Ok(manifest)
    }

    fn preflight_originals(&self, record: &TransactionRecord) -> Result<()> {
        for operation in &record.operations {
            let policy = &FILE_POLICY[operation.index];
            let target = self.target_path(policy)?;
            let current = validation::read_regular_bounded(&target, policy.max_bytes)?;
            if !matches_expected(
                current.as_ref().map(|value| value.0.as_slice()),
                &operation.original,
            ) {
                return Err(SnapshotError::ConcurrentModification(
                    policy.manifest_path.to_owned(),
                ));
            }
        }
        Ok(())
    }

    fn verify_applied_operation(&self, operation: &RestoreOperation) -> Result<()> {
        if operation.desired_state == SnapshotFileState::Omitted {
            return Ok(());
        }
        let policy = &FILE_POLICY[operation.index];
        let target = self.target_path(policy)?;
        let current = validation::read_regular_bounded(&target, policy.max_bytes)?;
        match (&operation.applied_sha256, current) {
            (Some(expected), Some((bytes, _))) if validation::sha256_hex(&bytes) == *expected => {
                Ok(())
            }
            (None, None) => Ok(()),
            _ => Err(SnapshotError::ConcurrentModification(
                policy.manifest_path.to_owned(),
            )),
        }
    }

    fn transaction_path(&self, ticket_id: &str) -> Result<PathBuf> {
        validation::validate_identifier(ticket_id)?;
        Ok(self.transaction_root().join(format!("{ticket_id}.json")))
    }

    fn write_transaction(&self, record: &TransactionRecord) -> Result<()> {
        validation::ensure_directory_tree(self.data_root(), self.transaction_root())?;
        let encoded = serde_json::to_vec_pretty(record).map_err(|error| {
            SnapshotError::InvalidState(format!("cannot encode restore transaction: {error}"))
        })?;
        if encoded.len() as u64 > MAX_TRANSACTION_BYTES {
            return Err(SnapshotError::Oversized {
                path: "restore transaction".to_owned(),
                size: encoded.len() as u64,
                limit: MAX_TRANSACTION_BYTES,
            });
        }
        validation::write_durable(&self.transaction_path(&record.ticket.ticket_id)?, &encoded)
    }

    fn read_matching_transaction(&self, ticket: &RestoreTicket) -> Result<TransactionRecord> {
        if ticket.schema != TRANSACTION_SCHEMA_VERSION || ticket.profile_name != self.profile_name()
        {
            return Err(SnapshotError::InvalidState(
                "restore ticket does not belong to this store".to_owned(),
            ));
        }
        let record = self.read_transaction(&ticket.ticket_id)?;
        if record.ticket != *ticket {
            return Err(SnapshotError::InvalidState(
                "restore ticket does not match durable transaction".to_owned(),
            ));
        }
        Ok(record)
    }

    fn read_transaction(&self, ticket_id: &str) -> Result<TransactionRecord> {
        let path = self.transaction_path(ticket_id)?;
        let Some((bytes, _)) = validation::read_regular_bounded(&path, MAX_TRANSACTION_BYTES)?
        else {
            return Err(SnapshotError::InvalidState(format!(
                "restore transaction not found: {ticket_id}"
            )));
        };
        let record: TransactionRecord = serde_json::from_slice(&bytes).map_err(|error| {
            SnapshotError::InvalidState(format!(
                "cannot parse restore transaction {ticket_id}: {error}"
            ))
        })?;
        validate_transaction_record(self, &record)?;
        Ok(record)
    }

    fn transaction_records(&self) -> Result<Vec<TransactionRecord>> {
        if !self.transaction_root().exists() {
            return Ok(Vec::new());
        }
        validation::validate_directory_tree(self.data_root(), self.transaction_root())?;
        let mut records = Vec::new();
        for entry in fs::read_dir(self.transaction_root()).map_err(|error| {
            SnapshotError::io(
                format!(
                    "read transaction directory {}",
                    self.transaction_root().display()
                ),
                error,
            )
        })? {
            let entry = entry
                .map_err(|error| SnapshotError::io("read transaction directory entry", error))?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if validation::is_generated_name(&name, ".tmp-write-") {
                let metadata = fs::symlink_metadata(&path)
                    .map_err(|error| SnapshotError::io("inspect transaction temporary", error))?;
                if metadata.is_file() && !validation::is_link_or_reparse(&metadata) {
                    // An unpublished write is not a transaction. Preserve it,
                    // even when partial, without interpreting its payload.
                    continue;
                }
                return Err(SnapshotError::UnsafePath(format!("unsafe transaction temporary: {}", path.display())));
            }
            let ticket_id = name.strip_suffix(".json").ok_or_else(|| {
                SnapshotError::UnsafePath(format!(
                    "unexpected transaction entry: {}",
                    path.display()
                ))
            })?;
            validation::validate_identifier(ticket_id)?;
            records.push(self.read_transaction(ticket_id)?);
            if records.len() > MAX_TRANSACTION_RECORDS {
                return Err(SnapshotError::Capacity(format!(
                    "restore transaction count exceeds {MAX_TRANSACTION_RECORDS}"
                )));
            }
        }
        Ok(records)
    }

    fn pending_transactions(&self) -> Result<Vec<TransactionSummary>> {
        let mut records: Vec<_> = self
            .transaction_records()?
            .into_iter()
            .filter(|record| {
                !matches!(
                    record.status,
                    RestoreStatus::Committed | RestoreStatus::RolledBack
                )
            })
            .map(|record| TransactionSummary {
                ticket: record.ticket,
                status: record.status,
                materialization_pending: record.materialization_pending,
            })
            .collect();
        records.sort_by_key(|record| record.ticket.created_unix_ms);
        Ok(records)
    }

    fn prune_terminal_transactions(&self) -> Result<()> {
        let mut records = self.transaction_records()?;
        if records.len() < MAX_TRANSACTION_RECORDS {
            return Ok(());
        }
        records.sort_by_key(|record| record.ticket.created_unix_ms);
        let remove_count = records.len() + 1 - MAX_TRANSACTION_RECORDS;
        let removable: Vec<_> = records
            .iter()
            .filter(|record| {
                matches!(
                    record.status,
                    RestoreStatus::Committed | RestoreStatus::RolledBack
                )
            })
            .take(remove_count)
            .collect();
        if removable.len() != remove_count {
            return Err(SnapshotError::Capacity(
                "restore transaction bound reached with active records".to_owned(),
            ));
        }
        for record in removable {
            let path = self.transaction_path(&record.ticket.ticket_id)?;
            fs::remove_file(&path).map_err(|error| {
                SnapshotError::io(format!("remove old transaction {}", path.display()), error)
            })?;
        }
        Ok(())
    }

    fn backup_directory(&self, ticket_id: &str) -> Result<PathBuf> {
        validation::validate_identifier(ticket_id)?;
        let directory = self.backup_root().join(ticket_id);
        if directory.starts_with(self.data_root()) {
            return Err(SnapshotError::UnsafePath(
                "transaction backup would be inside Nexus data root".to_owned(),
            ));
        }
        Ok(directory)
    }

    fn backup_file(&self, ticket_id: &str, index: usize) -> Result<PathBuf> {
        if index >= FILE_POLICY.len() {
            return Err(SnapshotError::InvalidPath(format!("backup index {index}")));
        }
        Ok(self
            .backup_directory(ticket_id)?
            .join(format!("{index}.original")))
    }

    fn rollback_discard_file(&self, ticket_id: &str, index: usize) -> Result<PathBuf> {
        if index >= FILE_POLICY.len() {
            return Err(SnapshotError::InvalidPath(format!("discard index {index}")));
        }
        Ok(self
            .backup_directory(ticket_id)?
            .join(format!("{index}.discarded")))
    }

    fn move_rollback_discard(
        &self,
        ticket_id: &str,
        index: usize,
        target: &Path,
        limit: u64,
        backup_directory: &Path,
        expected_applied_sha256: Option<&str>,
        confirmed_applied: bool,
    ) -> Result<()> {
        let discard = self.rollback_discard_file(ticket_id, index)?;
        let directory_present =
            validation::validate_optional_directory_tree(self.dsh_home(), backup_directory)?;
        let discarded = if directory_present {
            validation::read_regular_bounded(&discard, limit)?
        } else {
            None
        };
        let current = validation::read_regular_bounded(target, limit)?;
        if current.is_some() && discarded.is_some() {
            return Err(SnapshotError::Integrity(format!(
                "restore target and rollback discard both exist for {}",
                FILE_POLICY[index].manifest_path
            )));
        }
        let expected_matches = |bytes: &[u8]| {
            expected_applied_sha256
                .is_some_and(|expected| validation::sha256_hex(bytes) == expected)
        };
        if let Some((bytes, _)) = discarded {
            if !expected_matches(&bytes) {
                return Err(SnapshotError::Integrity(format!(
                    "rollback discard changed for {}",
                    FILE_POLICY[index].manifest_path
                )));
            }
            return Ok(());
        }
        let Some((bytes, _)) = current else {
            if confirmed_applied && expected_applied_sha256.is_some() {
                return Err(SnapshotError::Integrity(format!(
                    "applied target and rollback discard are both unavailable for {}",
                    FILE_POLICY[index].manifest_path
                )));
            }
            return Ok(());
        };
        if !expected_matches(&bytes) {
            return Err(SnapshotError::ConcurrentModification(
                FILE_POLICY[index].manifest_path.to_owned(),
            ));
        }
        validation::ensure_directory_tree(self.dsh_home(), backup_directory)?;
        validation::validate_directory_tree(self.dsh_home(), backup_directory)?;
        validation::rename_durable(target, &discard)
    }

    fn remove_backup_directory(&self, ticket_id: &str) -> Result<()> {
        let directory = self.backup_directory(ticket_id)?;
        if !validation::validate_optional_directory_tree(self.dsh_home(), &directory)? {
            return Ok(());
        }
        for entry in fs::read_dir(&directory).map_err(|error| {
            SnapshotError::io(
                format!("read backup directory {}", directory.display()),
                error,
            )
        })? {
            let entry = entry.map_err(|error| SnapshotError::io("read backup entry", error))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let base = name.strip_suffix(".deleted").unwrap_or(&name);
            let valid_name = [".original", ".discarded"].into_iter().any(|suffix| {
                base.strip_suffix(suffix)
                    .and_then(|index| index.parse::<usize>().ok())
                    .is_some_and(|index| index < FILE_POLICY.len())
            });
            if !valid_name {
                return Err(SnapshotError::UnsafePath(format!(
                    "unexpected transaction backup entry: {}",
                    entry.path().display()
                )));
            }
        }
        for index in 0..FILE_POLICY.len() {
            for suffix in ["original", "discarded"] {
                validation::validate_directory_tree(self.dsh_home(), &directory)?;
                let file = directory.join(format!("{index}.{suffix}"));
                let tombstone = directory.join(format!("{index}.{suffix}.deleted"));
                validation::remove_sensitive_file_durable(&file, &tombstone)?;
            }
        }
        validation::validate_directory_tree(self.dsh_home(), &directory)?;
        if fs::read_dir(&directory)
            .map_err(|error| {
                SnapshotError::io(
                    format!("read backup directory {}", directory.display()),
                    error,
                )
            })?
            .next()
            .is_some()
        {
            return Err(SnapshotError::UnsafePath(format!(
                "transaction backup did not become empty: {}",
                directory.display()
            )));
        }
        validation::remove_empty_directory_durable(&directory)?;
        if let Some(profile_root) = directory.parent() {
            if validation::validate_optional_directory_tree(self.dsh_home(), profile_root)?
                && fs::read_dir(profile_root)
                    .map_err(|error| {
                        SnapshotError::io(
                            format!("read backup profile root {}", profile_root.display()),
                            error,
                        )
                    })?
                    .next()
                    .is_none()
            {
                validation::remove_empty_directory_durable(profile_root)?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn apply_restore_with_fault(
        &self,
        ticket: &RestoreTicket,
        fail_after_operations: usize,
    ) -> Result<RestoreOutcome> {
        self.apply_restore_inner(ticket, Some(fail_after_operations), None)
    }

    #[cfg(test)]
    pub(crate) fn apply_restore_with_namespace_fault(&self, ticket: &RestoreTicket, index: usize) -> Result<RestoreOutcome> {
        self.apply_restore_inner(ticket, None, Some(RestoreFault::AfterNamespace(index)))
    }

    #[cfg(test)]
    pub(crate) fn rollback_restore_with_fault(
        &self,
        ticket: &RestoreTicket,
        fault: RestoreFault,
    ) -> Result<RestoreOutcome> {
        self.rollback_restore_inner(ticket, Some(fault))
    }

    #[cfg(test)]
    pub(crate) fn commit_restore_with_fault(
        &self,
        ticket: &RestoreTicket,
        fault: RestoreFault,
    ) -> Result<RestoreOutcome> {
        self.commit_restore_inner(ticket, Some(fault))
    }
}

fn validate_transaction_record(store: &SnapshotStore, record: &TransactionRecord) -> Result<()> {
    if record.schema != TRANSACTION_SCHEMA_VERSION
        || record.ticket.schema != TRANSACTION_SCHEMA_VERSION
        || record.ticket.profile_name != store.profile_name()
        || record.operations.len() != FILE_POLICY.len()
    {
        return Err(SnapshotError::InvalidState(
            "restore transaction schema/profile/operation count is invalid".to_owned(),
        ));
    }
    validation::validate_identifier(&record.ticket.ticket_id)?;
    validation::validate_identifier(&record.ticket.snapshot_id)?;
    validation::validate_sha256(&record.ticket.snapshot_manifest_sha256)?;
    for (expected_index, operation) in record.operations.iter().enumerate() {
        if operation.index != expected_index {
            return Err(SnapshotError::InvalidState(
                "restore transaction operation order is invalid".to_owned(),
            ));
        }
        if let Some(hash) = &operation.original.sha256 {
            validation::validate_sha256(hash)?;
        }
        if operation.original.present != operation.original.sha256.is_some()
            || (!operation.original.present && operation.original.size != 0)
            || operation.original.size > FILE_POLICY[expected_index].max_bytes
        {
            return Err(SnapshotError::InvalidState(format!(
                "restore transaction original metadata is invalid for {}",
                FILE_POLICY[expected_index].manifest_path
            )));
        }
        if let Some(hash) = &operation.desired_sha256 {
            validation::validate_sha256(hash)?;
        }
        if let Some(hash) = &operation.applied_sha256 {
            validation::validate_sha256(hash)?;
        }
        let desired_hash_expected = operation.desired_state == SnapshotFileState::Present;
        if desired_hash_expected != operation.desired_sha256.is_some() {
            return Err(SnapshotError::InvalidState(format!(
                "restore transaction desired metadata is invalid for {}",
                FILE_POLICY[expected_index].manifest_path
            )));
        }
        let applied_hash_expected = operation.status == OperationStatus::Applied
            && operation.desired_state == SnapshotFileState::Present;
        if applied_hash_expected != operation.applied_sha256.is_some() {
            return Err(SnapshotError::InvalidState(format!(
                "restore transaction applied metadata is invalid for {}",
                FILE_POLICY[expected_index].manifest_path
            )));
        }
    }
    Ok(())
}

fn matches_expected(current: Option<&[u8]>, expected: &ExpectedFile) -> bool {
    match (current, expected.present, expected.sha256.as_deref()) {
        (None, false, None) => true,
        (Some(bytes), true, Some(hash)) => {
            bytes.len() as u64 == expected.size && validation::sha256_hex(bytes) == hash
        }
        _ => false,
    }
}

fn maybe_inject_failure(fail_after: Option<usize>, completed: usize) -> Result<()> {
    if fail_after == Some(completed) {
        return Err(SnapshotError::InjectedFailure(format!(
            "after operation {completed}"
        )));
    }
    Ok(())
}

fn maybe_inject_restore_failure(
    requested: Option<RestoreFault>,
    reached: RestoreFault,
) -> Result<()> {
    if requested == Some(reached) {
        return Err(SnapshotError::InjectedFailure(format!("{reached:?}")));
    }
    Ok(())
}

fn outcome(record: &TransactionRecord) -> RestoreOutcome {
    RestoreOutcome {
        ticket_id: record.ticket.ticket_id.clone(),
        snapshot_id: record.ticket.snapshot_id.clone(),
        status: record.status,
        needs_materialization: record.ticket.needs_materialization,
        materialization_pending: record.materialization_pending,
    }
}
