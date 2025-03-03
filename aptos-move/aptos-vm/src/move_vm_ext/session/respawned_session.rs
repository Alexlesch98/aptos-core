// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{
    data_cache::StorageAdapter,
    move_vm_ext::{
        session::view_with_change_set::ExecutorViewWithChangeSet, AptosMoveResolver, SessionExt,
        SessionId,
    },
    AptosVM,
};
use aptos_gas_algebra::Fee;
use aptos_vm_types::{change_set::VMChangeSet, storage::change_set_configs::ChangeSetConfigs};
use move_core_types::vm_status::{err_msg, StatusCode, VMStatus};

fn unwrap_or_invariant_violation<T>(value: Option<T>, msg: &str) -> Result<T, VMStatus> {
    value
        .ok_or_else(|| VMStatus::error(StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR, err_msg(msg)))
}

/// We finish the session after the user transaction is done running to get the change set and
/// charge gas and storage fee based on it before running storage refunds and the transaction
/// epilogue. The latter needs to see the state view as if the change set is applied on top of
/// the base state view, and this struct implements that.
#[ouroboros::self_referencing]
pub struct RespawnedSession<'r, 'l> {
    executor_view: ExecutorViewWithChangeSet<'r>,
    #[borrows(executor_view)]
    #[covariant]
    resolver: StorageAdapter<'this, ExecutorViewWithChangeSet<'r>>,
    #[borrows(resolver)]
    #[not_covariant]
    session: Option<SessionExt<'this, 'l>>,
    pub storage_refund: Fee,
}

impl<'r, 'l> RespawnedSession<'r, 'l> {
    pub fn spawn(
        vm: &'l AptosVM,
        session_id: SessionId,
        base: &'r impl AptosMoveResolver,
        previous_session_change_set: VMChangeSet,
        storage_refund: Fee,
    ) -> Result<Self, VMStatus> {
        let executor_view = ExecutorViewWithChangeSet::new(
            base.as_executor_view(),
            base.as_resource_group_view(),
            previous_session_change_set,
        );

        Ok(RespawnedSessionBuilder {
            executor_view,
            resolver_builder: |executor_view| vm.as_move_resolver_with_group_view(executor_view),
            session_builder: |resolver| Some(vm.new_session(resolver, session_id)),
            storage_refund,
        }
        .build())
    }

    pub fn execute<T>(
        &mut self,
        fun: impl FnOnce(&mut SessionExt) -> Result<T, VMStatus>,
    ) -> Result<T, VMStatus> {
        self.with_session_mut(|session| {
            fun(unwrap_or_invariant_violation(
                session.as_mut(),
                "VM respawned session has to be set for execution.",
            )?)
        })
    }

    pub fn finish(
        mut self,
        change_set_configs: &ChangeSetConfigs,
    ) -> Result<VMChangeSet, VMStatus> {
        let additional_change_set = self.with_session_mut(|session| {
            unwrap_or_invariant_violation(
                session.take(),
                "VM session cannot be finished more than once.",
            )?
            .finish(change_set_configs)
            .map_err(|e| e.into_vm_status())
        })?;
        if additional_change_set.has_creation() {
            // After respawning, for example, in the epilogue, there shouldn't be new slots
            // created, otherwise there's a potential vulnerability like this:
            // 1. slot created by the user
            // 2. another user transaction deletes the slot and claims the refund
            // 3. in the epilogue the same slot gets recreated, and the final write set will have
            //    a ModifyWithMetadata carrying the original metadata
            // 4. user keeps doing the same and repeatedly claim refund out of the slot.
            return Err(VMStatus::error(
                StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR,
                err_msg("Unexpected storage allocation after respawning session."),
            ));
        }
        let mut change_set = self.into_heads().executor_view.change_set;
        change_set
            .squash_additional_change_set(additional_change_set, change_set_configs)
            .map_err(|_err| {
                VMStatus::error(
                    StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR,
                    err_msg("Failed to squash VMChangeSet"),
                )
            })?;
        Ok(change_set)
    }

    pub fn get_storage_fee_refund(&self) -> Fee {
        *self.borrow_storage_refund()
    }
}
