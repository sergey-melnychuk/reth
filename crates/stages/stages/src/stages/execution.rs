use crate::stages::MERKLE_STAGE_DEFAULT_INCREMENTAL_THRESHOLD;
use alloy_consensus::{BlockHeader, Header};
use alloy_primitives::BlockNumber;
use num_traits::Zero;
use reth_config::config::ExecutionConfig;
use reth_consensus::{ConsensusError, FullConsensus};
use reth_db::{static_file::HeaderMask, tables};
use reth_evm::{execute::Executor, metrics::ExecutorMetrics, ConfigureEvm};
use reth_execution_types::Chain;
use reth_exex::{ExExManagerHandle, ExExNotification, ExExNotificationSource};
use reth_primitives_traits::{format_gas_throughput, Block, BlockBody, NodePrimitives};
use reth_provider::{
    providers::{StaticFileProvider, StaticFileWriter},
    BlockHashReader, BlockReader, DBProvider, ExecutionOutcome, HeaderProvider,
    LatestStateProviderRef, OriginalValuesKnown, ProviderError, StateCommitmentProvider,
    StateWriter, StaticFileProviderFactory, StatsReader, StorageLocation, TransactionVariant,
};
use reth_revm::database::StateProviderDatabase;
use reth_stages_api::{
    BlockErrorKind, CheckpointBlockRange, EntitiesCheckpoint, ExecInput, ExecOutput,
    ExecutionCheckpoint, ExecutionStageThresholds, Stage, StageCheckpoint, StageError, StageId,
    UnwindInput, UnwindOutput,
};
use reth_static_file_types::StaticFileSegment;
use std::{
    cmp::Ordering,
    ops::RangeInclusive,
    sync::Arc,
    task::{ready, Context, Poll},
    time::{Duration, Instant},
};
use tracing::*;

use super::missing_static_data_error;

/// The execution stage executes all transactions and
/// update history indexes.
///
/// Input tables:
/// - [`tables::CanonicalHeaders`] get next block to execute.
/// - [`tables::Headers`] get for revm environment variables.
/// - [`tables::HeaderTerminalDifficulties`]
/// - [`tables::BlockBodyIndices`] to get tx number
/// - [`tables::Transactions`] to execute
///
/// For state access [`LatestStateProviderRef`] provides us latest state and history state
/// For latest most recent state [`LatestStateProviderRef`] would need (Used for execution Stage):
/// - [`tables::PlainAccountState`]
/// - [`tables::Bytecodes`]
/// - [`tables::PlainStorageState`]
///
/// Tables updated after state finishes execution:
/// - [`tables::PlainAccountState`]
/// - [`tables::PlainStorageState`]
/// - [`tables::Bytecodes`]
/// - [`tables::AccountChangeSets`]
/// - [`tables::StorageChangeSets`]
///
/// For unwinds we are accessing:
/// - [`tables::BlockBodyIndices`] get tx index to know what needs to be unwinded
/// - [`tables::AccountsHistory`] to remove change set and apply old values to
/// - [`tables::PlainAccountState`] [`tables::StoragesHistory`] to remove change set and apply old
///   values to [`tables::PlainStorageState`]
// false positive, we cannot derive it if !DB: Debug.
#[derive(Debug)]
pub struct ExecutionStage<E>
where
    E: ConfigureEvm,
{
    /// The stage's internal block executor
    evm_config: E,
    /// The consensus instance for validating blocks.
    consensus: Arc<dyn FullConsensus<E::Primitives, Error = ConsensusError>>,
    /// The commit thresholds of the execution stage.
    thresholds: ExecutionStageThresholds,
    /// The highest threshold (in number of blocks) for switching between incremental
    /// and full calculations across [`super::MerkleStage`], [`super::AccountHashingStage`] and
    /// [`super::StorageHashingStage`]. This is required to figure out if can prune or not
    /// changesets on subsequent pipeline runs.
    external_clean_threshold: u64,
    /// Input for the post execute commit hook.
    /// Set after every [`ExecutionStage::execute`] and cleared after
    /// [`ExecutionStage::post_execute_commit`].
    post_execute_commit_input: Option<Chain<E::Primitives>>,
    /// Input for the post unwind commit hook.
    /// Set after every [`ExecutionStage::unwind`] and cleared after
    /// [`ExecutionStage::post_unwind_commit`].
    post_unwind_commit_input: Option<Chain<E::Primitives>>,
    /// Handle to communicate with `ExEx` manager.
    exex_manager_handle: ExExManagerHandle<E::Primitives>,
    /// Executor metrics.
    metrics: ExecutorMetrics,
}

impl<E> ExecutionStage<E>
where
    E: ConfigureEvm,
{
    /// Create new execution stage with specified config.
    pub fn new(
        evm_config: E,
        consensus: Arc<dyn FullConsensus<E::Primitives, Error = ConsensusError>>,
        thresholds: ExecutionStageThresholds,
        external_clean_threshold: u64,
        exex_manager_handle: ExExManagerHandle<E::Primitives>,
    ) -> Self {
        Self {
            external_clean_threshold,
            evm_config,
            consensus,
            thresholds,
            post_execute_commit_input: None,
            post_unwind_commit_input: None,
            exex_manager_handle,
            metrics: ExecutorMetrics::default(),
        }
    }

    /// Create an execution stage with the provided executor.
    ///
    /// The commit threshold will be set to [`MERKLE_STAGE_DEFAULT_INCREMENTAL_THRESHOLD`].
    pub fn new_with_executor(
        evm_config: E,
        consensus: Arc<dyn FullConsensus<E::Primitives, Error = ConsensusError>>,
    ) -> Self {
        Self::new(
            evm_config,
            consensus,
            ExecutionStageThresholds::default(),
            MERKLE_STAGE_DEFAULT_INCREMENTAL_THRESHOLD,
            ExExManagerHandle::empty(),
        )
    }

    /// Create new instance of [`ExecutionStage`] from configuration.
    pub fn from_config(
        evm_config: E,
        consensus: Arc<dyn FullConsensus<E::Primitives, Error = ConsensusError>>,
        config: ExecutionConfig,
        external_clean_threshold: u64,
    ) -> Self {
        Self::new(
            evm_config,
            consensus,
            config.into(),
            external_clean_threshold,
            ExExManagerHandle::empty(),
        )
    }

    /// Returns whether we can perform pruning of [`tables::AccountChangeSets`] and
    /// [`tables::StorageChangeSets`].
    ///
    /// This function verifies whether the [`super::MerkleStage`] or Hashing stages will run from
    /// scratch. If at least one stage isn't starting anew, it implies that pruning of
    /// changesets cannot occur. This is determined by checking the highest clean threshold
    /// (`self.external_clean_threshold`) across the stages.
    ///
    /// Given that `start_block` changes with each checkpoint, it's necessary to inspect
    /// [`tables::AccountsTrie`] to ensure that [`super::MerkleStage`] hasn't
    /// been previously executed.
    fn can_prune_changesets(
        &self,
        provider: impl StatsReader,
        start_block: u64,
        max_block: u64,
    ) -> Result<bool, StageError> {
        // We can only prune changesets if we're not executing MerkleStage from scratch (by
        // threshold or first-sync)
        Ok(max_block - start_block > self.external_clean_threshold ||
            provider.count_entries::<tables::AccountsTrie>()?.is_zero())
    }

    /// Performs consistency check on static files.
    ///
    /// This function compares the highest receipt number recorded in the database with that in the
    /// static file to detect any discrepancies due to unexpected shutdowns or database rollbacks.
    /// **If the height in the static file is higher**, it rolls back (unwinds) the static file.
    /// **Conversely, if the height in the database is lower**, it triggers a rollback in the
    /// database (by returning [`StageError`]) until the heights in both the database and static
    /// file match.
    fn ensure_consistency<Provider>(
        &self,
        provider: &Provider,
        checkpoint: u64,
        unwind_to: Option<u64>,
    ) -> Result<(), StageError>
    where
        Provider: StaticFileProviderFactory + DBProvider + BlockReader + HeaderProvider,
    {
        // If there's any receipts pruning configured, receipts are written directly to database and
        // inconsistencies are expected.
        if provider.prune_modes_ref().has_receipts_pruning() {
            return Ok(())
        }

        // Get next expected receipt number
        let next_receipt_num =
            provider.block_body_indices(checkpoint)?.map(|b| b.next_tx_num()).unwrap_or(0);

        let static_file_provider = provider.static_file_provider();

        // Get next expected receipt number in static files
        let next_static_file_receipt_num = static_file_provider
            .get_highest_static_file_tx(StaticFileSegment::Receipts)
            .map(|num| num + 1)
            .unwrap_or(0);

        // Check if we had any unexpected shutdown after committing to static files, but
        // NOT committing to database.
        match next_static_file_receipt_num.cmp(&next_receipt_num) {
            // It can be equal when it's a chain of empty blocks, but we still need to update the
            // last block in the range.
            Ordering::Greater | Ordering::Equal => {
                let mut static_file_producer =
                    static_file_provider.latest_writer(StaticFileSegment::Receipts)?;
                static_file_producer
                    .prune_receipts(next_static_file_receipt_num - next_receipt_num, checkpoint)?;
                // Since this is a database <-> static file inconsistency, we commit the change
                // straight away.
                static_file_producer.commit()?;
            }
            Ordering::Less => {
                // If we are already in the process of unwind, this might be fine because we will
                // fix the inconsistency right away.
                if let Some(unwind_to) = unwind_to {
                    let next_receipt_num_after_unwind = provider
                        .block_body_indices(unwind_to)?
                        .map(|b| b.next_tx_num())
                        .ok_or(ProviderError::BlockBodyIndicesNotFound(unwind_to))?;

                    if next_receipt_num_after_unwind > next_static_file_receipt_num {
                        // This means we need a deeper unwind.
                    } else {
                        return Ok(())
                    }
                }

                return Err(missing_static_data_error(
                    next_static_file_receipt_num.saturating_sub(1),
                    &static_file_provider,
                    provider,
                    StaticFileSegment::Receipts,
                )?)
            }
        }

        Ok(())
    }
}

impl<E, Provider> Stage<Provider> for ExecutionStage<E>
where
    E: ConfigureEvm,
    Provider: DBProvider
        + BlockReader<
            Block = <E::Primitives as NodePrimitives>::Block,
            Header = <E::Primitives as NodePrimitives>::BlockHeader,
        > + StaticFileProviderFactory
        + StatsReader
        + BlockHashReader
        + StateWriter<Receipt = <E::Primitives as NodePrimitives>::Receipt>
        + StateCommitmentProvider,
{
    /// Return the id of the stage
    fn id(&self) -> StageId {
        StageId::Execution
    }

    fn poll_execute_ready(
        &mut self,
        cx: &mut Context<'_>,
        _: ExecInput,
    ) -> Poll<Result<(), StageError>> {
        ready!(self.exex_manager_handle.poll_ready(cx));

        Poll::Ready(Ok(()))
    }

    /// Execute the stage
    fn execute(&mut self, provider: &Provider, input: ExecInput) -> Result<ExecOutput, StageError> {
        if input.target_reached() {
            return Ok(ExecOutput::done(input.checkpoint()))
        }

        let start_block = input.next_block();
        let max_block = input.target();
        let static_file_provider = provider.static_file_provider();

        self.ensure_consistency(provider, input.checkpoint().block_number, None)?;

        let db = StateProviderDatabase(LatestStateProviderRef::new(provider));
        let mut executor = self.evm_config.batch_executor(db);

        // Progress tracking
        let mut stage_progress = start_block;
        let mut stage_checkpoint = execution_checkpoint(
            &static_file_provider,
            start_block,
            max_block,
            input.checkpoint(),
        )?;

        let mut fetch_block_duration = Duration::default();
        let mut execution_duration = Duration::default();

        let mut last_block = start_block;
        let mut last_execution_duration = Duration::default();
        let mut last_cumulative_gas = 0;
        let mut last_log_instant = Instant::now();
        let log_duration = Duration::from_secs(10);

        debug!(target: "sync::stages::execution", start = start_block, end = max_block, "Executing range");

        // Execute block range
        let mut cumulative_gas = 0;
        let batch_start = Instant::now();

        let mut blocks = Vec::new();
        let mut results = Vec::new();
        for block_number in start_block..=max_block {
            // Fetch the block
            let fetch_block_start = Instant::now();

            // we need the block's transactions but we don't need the transaction hashes
            let block = provider
                .recovered_block(block_number.into(), TransactionVariant::NoHash)?
                .ok_or_else(|| ProviderError::HeaderNotFound(block_number.into()))?;

            fetch_block_duration += fetch_block_start.elapsed();

            cumulative_gas += block.header().gas_used();

            // Configure the executor to use the current state.
            trace!(target: "sync::stages::execution", number = block_number, txs = block.body().transactions().len(), "Executing block");

            // Execute the block
            let execute_start = Instant::now();

            let result = self.metrics.metered_one(&block, |input| {
                executor.execute_one(input).map_err(|error| StageError::Block {
                    block: Box::new(block.block_with_parent()),
                    error: BlockErrorKind::Execution(error),
                })
            })?;

            if let Err(err) = self.consensus.validate_block_post_execution(&block, &result) {
                return Err(StageError::Block {
                    block: Box::new(block.block_with_parent()),
                    error: BlockErrorKind::Validation(err),
                })
            }
            results.push(result);

            execution_duration += execute_start.elapsed();

            // Log execution throughput
            if last_log_instant.elapsed() >= log_duration {
                info!(
                    target: "sync::stages::execution",
                    start = last_block,
                    end = block_number,
                    throughput = format_gas_throughput(cumulative_gas - last_cumulative_gas, execution_duration - last_execution_duration),
                    "Executed block range"
                );

                last_block = block_number + 1;
                last_execution_duration = execution_duration;
                last_cumulative_gas = cumulative_gas;
                last_log_instant = Instant::now();
            }

            stage_progress = block_number;
            stage_checkpoint.progress.processed += block.header().gas_used();

            // If we have ExExes we need to save the block in memory for later
            if self.exex_manager_handle.has_exexs() {
                blocks.push(block);
            }

            // Check if we should commit now
            if self.thresholds.is_end_of_batch(
                block_number - start_block,
                executor.size_hint() as u64,
                cumulative_gas,
                batch_start.elapsed(),
            ) {
                break
            }
        }

        // prepare execution output for writing
        let time = Instant::now();
        let mut state = ExecutionOutcome::from_blocks(
            start_block,
            executor.into_state().take_bundle(),
            results,
        );
        let write_preparation_duration = time.elapsed();

        // log the gas per second for the range we just executed
        debug!(
            target: "sync::stages::execution",
            start = start_block,
            end = stage_progress,
            throughput = format_gas_throughput(cumulative_gas, execution_duration),
            "Finished executing block range"
        );

        // Prepare the input for post execute commit hook, where an `ExExNotification` will be sent.
        //
        // Note: Since we only write to `blocks` if there are any ExExes, we don't need to perform
        // the `has_exexs` check here as well
        if !blocks.is_empty() {
            let previous_input =
                self.post_execute_commit_input.replace(Chain::new(blocks, state.clone(), None));

            if previous_input.is_some() {
                // Not processing the previous post execute commit input is a critical error, as it
                // means that we didn't send the notification to ExExes
                return Err(StageError::PostExecuteCommit(
                    "Previous post execute commit input wasn't processed",
                ))
            }
        }

        let time = Instant::now();

        if self.can_prune_changesets(provider, start_block, max_block)? {
            let prune_modes = provider.prune_modes_ref();

            // Iterate over all reverts and clear them if pruning is configured.
            for block_number in start_block..=max_block {
                let Some(reverts) =
                    state.bundle.reverts.get_mut((block_number - start_block) as usize)
                else {
                    break
                };

                // If both account history and storage history pruning is configured, clear reverts
                // for this block.
                if prune_modes
                    .account_history
                    .is_some_and(|m| m.should_prune(block_number, max_block)) &&
                    prune_modes
                        .storage_history
                        .is_some_and(|m| m.should_prune(block_number, max_block))
                {
                    reverts.clear();
                }
            }
        }

        // write output
        provider.write_state(&state, OriginalValuesKnown::Yes, StorageLocation::StaticFiles)?;

        let db_write_duration = time.elapsed();
        debug!(
            target: "sync::stages::execution",
            block_fetch = ?fetch_block_duration,
            execution = ?execution_duration,
            write_preparation = ?write_preparation_duration,
            write = ?db_write_duration,
            "Execution time"
        );

        let done = stage_progress == max_block;
        Ok(ExecOutput {
            checkpoint: StageCheckpoint::new(stage_progress)
                .with_execution_stage_checkpoint(stage_checkpoint),
            done,
        })
    }

    fn post_execute_commit(&mut self) -> Result<(), StageError> {
        let Some(chain) = self.post_execute_commit_input.take() else { return Ok(()) };

        // NOTE: We can ignore the error here, since an error means that the channel is closed,
        // which means the manager has died, which then in turn means the node is shutting down.
        let _ = self.exex_manager_handle.send(
            ExExNotificationSource::Pipeline,
            ExExNotification::ChainCommitted { new: Arc::new(chain) },
        );

        Ok(())
    }

    /// Unwind the stage.
    fn unwind(
        &mut self,
        provider: &Provider,
        input: UnwindInput,
    ) -> Result<UnwindOutput, StageError> {
        let (range, unwind_to, _) =
            input.unwind_block_range_with_threshold(self.thresholds.max_blocks.unwrap_or(u64::MAX));
        if range.is_empty() {
            return Ok(UnwindOutput {
                checkpoint: input.checkpoint.with_block_number(input.unwind_to),
            })
        }

        self.ensure_consistency(provider, input.checkpoint.block_number, Some(unwind_to))?;

        // Unwind account and storage changesets, as well as receipts.
        //
        // This also updates `PlainStorageState` and `PlainAccountState`.
        let bundle_state_with_receipts =
            provider.take_state_above(unwind_to, StorageLocation::Both)?;

        // Prepare the input for post unwind commit hook, where an `ExExNotification` will be sent.
        if self.exex_manager_handle.has_exexs() {
            // Get the blocks for the unwound range.
            let blocks = provider.recovered_block_range(range.clone())?;
            let previous_input = self.post_unwind_commit_input.replace(Chain::new(
                blocks,
                bundle_state_with_receipts,
                None,
            ));

            debug_assert!(
                previous_input.is_none(),
                "Previous post unwind commit input wasn't processed"
            );
            if let Some(previous_input) = previous_input {
                tracing::debug!(target: "sync::stages::execution", ?previous_input, "Previous post unwind commit input wasn't processed");
            }
        }

        // Update the checkpoint.
        let mut stage_checkpoint = input.checkpoint.execution_stage_checkpoint();
        if let Some(stage_checkpoint) = stage_checkpoint.as_mut() {
            for block_number in range {
                stage_checkpoint.progress.processed -= provider
                    .block_by_number(block_number)?
                    .ok_or_else(|| ProviderError::HeaderNotFound(block_number.into()))?
                    .header()
                    .gas_used();
            }
        }
        let checkpoint = if let Some(stage_checkpoint) = stage_checkpoint {
            StageCheckpoint::new(unwind_to).with_execution_stage_checkpoint(stage_checkpoint)
        } else {
            StageCheckpoint::new(unwind_to)
        };

        Ok(UnwindOutput { checkpoint })
    }

    fn post_unwind_commit(&mut self) -> Result<(), StageError> {
        let Some(chain) = self.post_unwind_commit_input.take() else { return Ok(()) };

        // NOTE: We can ignore the error here, since an error means that the channel is closed,
        // which means the manager has died, which then in turn means the node is shutting down.
        let _ = self.exex_manager_handle.send(
            ExExNotificationSource::Pipeline,
            ExExNotification::ChainReverted { old: Arc::new(chain) },
        );

        Ok(())
    }
}

fn execution_checkpoint<N: NodePrimitives>(
    provider: &StaticFileProvider<N>,
    start_block: BlockNumber,
    max_block: BlockNumber,
    checkpoint: StageCheckpoint,
) -> Result<ExecutionCheckpoint, ProviderError> {
    Ok(match checkpoint.execution_stage_checkpoint() {
        // If checkpoint block range fully matches our range,
        // we take the previously used stage checkpoint as-is.
        Some(stage_checkpoint @ ExecutionCheckpoint { block_range, .. })
            if block_range == CheckpointBlockRange::from(start_block..=max_block) =>
        {
            stage_checkpoint
        }
        // If checkpoint block range precedes our range seamlessly, we take the previously used
        // stage checkpoint and add the amount of gas from our range to the checkpoint total.
        Some(ExecutionCheckpoint {
            block_range: CheckpointBlockRange { to, .. },
            progress: EntitiesCheckpoint { processed, total },
        }) if to == start_block - 1 => ExecutionCheckpoint {
            block_range: CheckpointBlockRange { from: start_block, to: max_block },
            progress: EntitiesCheckpoint {
                processed,
                total: total + calculate_gas_used_from_headers(provider, start_block..=max_block)?,
            },
        },
        // If checkpoint block range ends on the same block as our range, we take the previously
        // used stage checkpoint.
        Some(ExecutionCheckpoint { block_range: CheckpointBlockRange { to, .. }, progress })
            if to == max_block =>
        {
            ExecutionCheckpoint {
                block_range: CheckpointBlockRange { from: start_block, to: max_block },
                progress,
            }
        }
        // If there's any other non-empty checkpoint, we calculate the remaining amount of total gas
        // to be processed not including the checkpoint range.
        Some(ExecutionCheckpoint { progress: EntitiesCheckpoint { processed, .. }, .. }) => {
            let after_checkpoint_block_number =
                calculate_gas_used_from_headers(provider, checkpoint.block_number + 1..=max_block)?;

            ExecutionCheckpoint {
                block_range: CheckpointBlockRange { from: start_block, to: max_block },
                progress: EntitiesCheckpoint {
                    processed,
                    total: processed + after_checkpoint_block_number,
                },
            }
        }
        // Otherwise, we recalculate the whole stage checkpoint including the amount of gas
        // already processed, if there's any.
        _ => {
            let processed = calculate_gas_used_from_headers(provider, 0..=start_block - 1)?;

            ExecutionCheckpoint {
                block_range: CheckpointBlockRange { from: start_block, to: max_block },
                progress: EntitiesCheckpoint {
                    processed,
                    total: processed +
                        calculate_gas_used_from_headers(provider, start_block..=max_block)?,
                },
            }
        }
    })
}

/// Calculates the total amount of gas used from the headers in the given range.
pub fn calculate_gas_used_from_headers<N: NodePrimitives>(
    provider: &StaticFileProvider<N>,
    range: RangeInclusive<BlockNumber>,
) -> Result<u64, ProviderError> {
    debug!(target: "sync::stages::execution", ?range, "Calculating gas used from headers");

    let mut gas_total = 0;

    let start = Instant::now();

    for entry in provider.fetch_range_iter(
        StaticFileSegment::Headers,
        *range.start()..*range.end() + 1,
        |cursor, number| cursor.get_one::<HeaderMask<Header>>(number.into()),
    )? {
        let Header { gas_used, .. } = entry?;
        gas_total += gas_used;
    }

    let duration = start.elapsed();
    debug!(target: "sync::stages::execution", ?range, ?duration, "Finished calculating gas used from headers");

    Ok(gas_total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{stages::MERKLE_STAGE_DEFAULT_REBUILD_THRESHOLD, test_utils::TestStageDB};
    use alloy_primitives::{address, hex_literal::hex, keccak256, Address, B256, U256};
    use alloy_rlp::Decodable;
    use assert_matches::assert_matches;
    use reth_chainspec::ChainSpecBuilder;
    use reth_db_api::{
        models::AccountBeforeTx,
        transaction::{DbTx, DbTxMut},
    };
    use reth_ethereum_consensus::EthBeaconConsensus;
    use reth_ethereum_primitives::Block;
    use reth_evm_ethereum::EthEvmConfig;
    use reth_primitives_traits::{Account, Bytecode, SealedBlock, StorageEntry};
    use reth_provider::{
        test_utils::create_test_provider_factory, AccountReader, DatabaseProviderFactory,
        ReceiptProvider, StaticFileProviderFactory,
    };
    use reth_prune::PruneModes;
    use reth_prune_types::{PruneMode, ReceiptsLogPruneConfig};
    use reth_stages_api::StageUnitCheckpoint;
    use std::collections::BTreeMap;

    fn stage() -> ExecutionStage<EthEvmConfig> {
        let evm_config =
            EthEvmConfig::new(Arc::new(ChainSpecBuilder::mainnet().berlin_activated().build()));
        let consensus = Arc::new(EthBeaconConsensus::new(Arc::new(
            ChainSpecBuilder::mainnet().berlin_activated().build(),
        )));
        ExecutionStage::new(
            evm_config,
            consensus,
            ExecutionStageThresholds {
                max_blocks: Some(100),
                max_changes: None,
                max_cumulative_gas: None,
                max_duration: None,
            },
            MERKLE_STAGE_DEFAULT_REBUILD_THRESHOLD,
            ExExManagerHandle::empty(),
        )
    }

    #[test]
    fn execution_checkpoint_matches() {
        let factory = create_test_provider_factory();

        let previous_stage_checkpoint = ExecutionCheckpoint {
            block_range: CheckpointBlockRange { from: 0, to: 0 },
            progress: EntitiesCheckpoint { processed: 1, total: 2 },
        };
        let previous_checkpoint = StageCheckpoint {
            block_number: 0,
            stage_checkpoint: Some(StageUnitCheckpoint::Execution(previous_stage_checkpoint)),
        };

        let stage_checkpoint = execution_checkpoint(
            &factory.static_file_provider(),
            previous_stage_checkpoint.block_range.from,
            previous_stage_checkpoint.block_range.to,
            previous_checkpoint,
        );

        assert!(
            matches!(stage_checkpoint, Ok(checkpoint) if checkpoint == previous_stage_checkpoint)
        );
    }

    #[test]
    fn execution_checkpoint_precedes() {
        let factory = create_test_provider_factory();
        let provider = factory.provider_rw().unwrap();

        let mut genesis_rlp = hex!("f901faf901f5a00000000000000000000000000000000000000000000000000000000000000000a01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347942adc25665018aa1fe0e6bc666dac8fc2697ff9baa045571b40ae66ca7480791bbb2887286e4e4c4b1b298b191c889d6959023a32eda056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421b901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000083020000808502540be400808000a00000000000000000000000000000000000000000000000000000000000000000880000000000000000c0c0").as_slice();
        let genesis = SealedBlock::<Block>::decode(&mut genesis_rlp).unwrap();
        let mut block_rlp = hex!("f90262f901f9a075c371ba45999d87f4542326910a11af515897aebce5265d3f6acd1f1161f82fa01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347942adc25665018aa1fe0e6bc666dac8fc2697ff9baa098f2dcd87c8ae4083e7017a05456c14eea4b1db2032126e27b3b1563d57d7cc0a08151d548273f6683169524b66ca9fe338b9ce42bc3540046c828fd939ae23bcba03f4e5c2ec5b2170b711d97ee755c160457bb58d8daa338e835ec02ae6860bbabb901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000083020000018502540be40082a8798203e800a00000000000000000000000000000000000000000000000000000000000000000880000000000000000f863f861800a8405f5e10094100000000000000000000000000000000000000080801ba07e09e26678ed4fac08a249ebe8ed680bf9051a5e14ad223e4b2b9d26e0208f37a05f6e3f188e3e6eab7d7d3b6568f5eac7d687b08d307d3154ccd8c87b4630509bc0").as_slice();
        let block = SealedBlock::<Block>::decode(&mut block_rlp).unwrap();
        provider.insert_historical_block(genesis.try_recover().unwrap()).unwrap();
        provider.insert_historical_block(block.clone().try_recover().unwrap()).unwrap();
        provider
            .static_file_provider()
            .latest_writer(StaticFileSegment::Headers)
            .unwrap()
            .commit()
            .unwrap();
        provider.commit().unwrap();

        let previous_stage_checkpoint = ExecutionCheckpoint {
            block_range: CheckpointBlockRange { from: 0, to: 0 },
            progress: EntitiesCheckpoint { processed: 1, total: 1 },
        };
        let previous_checkpoint = StageCheckpoint {
            block_number: 1,
            stage_checkpoint: Some(StageUnitCheckpoint::Execution(previous_stage_checkpoint)),
        };

        let stage_checkpoint =
            execution_checkpoint(&factory.static_file_provider(), 1, 1, previous_checkpoint);

        assert_matches!(stage_checkpoint, Ok(ExecutionCheckpoint {
            block_range: CheckpointBlockRange { from: 1, to: 1 },
            progress: EntitiesCheckpoint {
                processed,
                total
            }
        }) if processed == previous_stage_checkpoint.progress.processed &&
            total == previous_stage_checkpoint.progress.total + block.gas_used);
    }

    #[test]
    fn execution_checkpoint_recalculate_full_previous_some() {
        let factory = create_test_provider_factory();
        let provider = factory.provider_rw().unwrap();

        let mut genesis_rlp = hex!("f901faf901f5a00000000000000000000000000000000000000000000000000000000000000000a01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347942adc25665018aa1fe0e6bc666dac8fc2697ff9baa045571b40ae66ca7480791bbb2887286e4e4c4b1b298b191c889d6959023a32eda056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421b901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000083020000808502540be400808000a00000000000000000000000000000000000000000000000000000000000000000880000000000000000c0c0").as_slice();
        let genesis = SealedBlock::<Block>::decode(&mut genesis_rlp).unwrap();
        let mut block_rlp = hex!("f90262f901f9a075c371ba45999d87f4542326910a11af515897aebce5265d3f6acd1f1161f82fa01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347942adc25665018aa1fe0e6bc666dac8fc2697ff9baa098f2dcd87c8ae4083e7017a05456c14eea4b1db2032126e27b3b1563d57d7cc0a08151d548273f6683169524b66ca9fe338b9ce42bc3540046c828fd939ae23bcba03f4e5c2ec5b2170b711d97ee755c160457bb58d8daa338e835ec02ae6860bbabb901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000083020000018502540be40082a8798203e800a00000000000000000000000000000000000000000000000000000000000000000880000000000000000f863f861800a8405f5e10094100000000000000000000000000000000000000080801ba07e09e26678ed4fac08a249ebe8ed680bf9051a5e14ad223e4b2b9d26e0208f37a05f6e3f188e3e6eab7d7d3b6568f5eac7d687b08d307d3154ccd8c87b4630509bc0").as_slice();
        let block = SealedBlock::<Block>::decode(&mut block_rlp).unwrap();
        provider.insert_historical_block(genesis.try_recover().unwrap()).unwrap();
        provider.insert_historical_block(block.clone().try_recover().unwrap()).unwrap();
        provider
            .static_file_provider()
            .latest_writer(StaticFileSegment::Headers)
            .unwrap()
            .commit()
            .unwrap();
        provider.commit().unwrap();

        let previous_stage_checkpoint = ExecutionCheckpoint {
            block_range: CheckpointBlockRange { from: 0, to: 0 },
            progress: EntitiesCheckpoint { processed: 1, total: 1 },
        };
        let previous_checkpoint = StageCheckpoint {
            block_number: 1,
            stage_checkpoint: Some(StageUnitCheckpoint::Execution(previous_stage_checkpoint)),
        };

        let stage_checkpoint =
            execution_checkpoint(&factory.static_file_provider(), 1, 1, previous_checkpoint);

        assert_matches!(stage_checkpoint, Ok(ExecutionCheckpoint {
            block_range: CheckpointBlockRange { from: 1, to: 1 },
            progress: EntitiesCheckpoint {
                processed,
                total
            }
        }) if processed == previous_stage_checkpoint.progress.processed &&
            total == previous_stage_checkpoint.progress.total + block.gas_used());
    }

    #[test]
    fn execution_checkpoint_recalculate_full_previous_none() {
        let factory = create_test_provider_factory();
        let provider = factory.provider_rw().unwrap();

        let mut genesis_rlp = hex!("f901faf901f5a00000000000000000000000000000000000000000000000000000000000000000a01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347942adc25665018aa1fe0e6bc666dac8fc2697ff9baa045571b40ae66ca7480791bbb2887286e4e4c4b1b298b191c889d6959023a32eda056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421b901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000083020000808502540be400808000a00000000000000000000000000000000000000000000000000000000000000000880000000000000000c0c0").as_slice();
        let genesis = SealedBlock::<Block>::decode(&mut genesis_rlp).unwrap();
        let mut block_rlp = hex!("f90262f901f9a075c371ba45999d87f4542326910a11af515897aebce5265d3f6acd1f1161f82fa01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347942adc25665018aa1fe0e6bc666dac8fc2697ff9baa098f2dcd87c8ae4083e7017a05456c14eea4b1db2032126e27b3b1563d57d7cc0a08151d548273f6683169524b66ca9fe338b9ce42bc3540046c828fd939ae23bcba03f4e5c2ec5b2170b711d97ee755c160457bb58d8daa338e835ec02ae6860bbabb901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000083020000018502540be40082a8798203e800a00000000000000000000000000000000000000000000000000000000000000000880000000000000000f863f861800a8405f5e10094100000000000000000000000000000000000000080801ba07e09e26678ed4fac08a249ebe8ed680bf9051a5e14ad223e4b2b9d26e0208f37a05f6e3f188e3e6eab7d7d3b6568f5eac7d687b08d307d3154ccd8c87b4630509bc0").as_slice();
        let block = SealedBlock::<Block>::decode(&mut block_rlp).unwrap();
        provider.insert_historical_block(genesis.try_recover().unwrap()).unwrap();
        provider.insert_historical_block(block.clone().try_recover().unwrap()).unwrap();
        provider
            .static_file_provider()
            .latest_writer(StaticFileSegment::Headers)
            .unwrap()
            .commit()
            .unwrap();
        provider.commit().unwrap();

        let previous_checkpoint = StageCheckpoint { block_number: 1, stage_checkpoint: None };

        let stage_checkpoint =
            execution_checkpoint(&factory.static_file_provider(), 1, 1, previous_checkpoint);

        assert_matches!(stage_checkpoint, Ok(ExecutionCheckpoint {
            block_range: CheckpointBlockRange { from: 1, to: 1 },
            progress: EntitiesCheckpoint {
                processed: 0,
                total
            }
        }) if total == block.gas_used);
    }

    #[tokio::test]
    async fn sanity_execution_of_block() {
        let factory = create_test_provider_factory();
        let provider = factory.provider_rw().unwrap();
        let input = ExecInput { target: Some(1), checkpoint: None };
        let mut genesis_rlp = hex!("f901faf901f5a00000000000000000000000000000000000000000000000000000000000000000a01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347942adc25665018aa1fe0e6bc666dac8fc2697ff9baa045571b40ae66ca7480791bbb2887286e4e4c4b1b298b191c889d6959023a32eda056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421b901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000083020000808502540be400808000a00000000000000000000000000000000000000000000000000000000000000000880000000000000000c0c0").as_slice();
        let genesis = SealedBlock::<Block>::decode(&mut genesis_rlp).unwrap();
        let mut block_rlp = hex!("f90262f901f9a075c371ba45999d87f4542326910a11af515897aebce5265d3f6acd1f1161f82fa01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347942adc25665018aa1fe0e6bc666dac8fc2697ff9baa098f2dcd87c8ae4083e7017a05456c14eea4b1db2032126e27b3b1563d57d7cc0a08151d548273f6683169524b66ca9fe338b9ce42bc3540046c828fd939ae23bcba03f4e5c2ec5b2170b711d97ee755c160457bb58d8daa338e835ec02ae6860bbabb901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000083020000018502540be40082a8798203e800a00000000000000000000000000000000000000000000000000000000000000000880000000000000000f863f861800a8405f5e10094100000000000000000000000000000000000000080801ba07e09e26678ed4fac08a249ebe8ed680bf9051a5e14ad223e4b2b9d26e0208f37a05f6e3f188e3e6eab7d7d3b6568f5eac7d687b08d307d3154ccd8c87b4630509bc0").as_slice();
        let block = SealedBlock::<Block>::decode(&mut block_rlp).unwrap();
        provider.insert_historical_block(genesis.try_recover().unwrap()).unwrap();
        provider.insert_historical_block(block.clone().try_recover().unwrap()).unwrap();
        provider
            .static_file_provider()
            .latest_writer(StaticFileSegment::Headers)
            .unwrap()
            .commit()
            .unwrap();
        {
            let static_file_provider = provider.static_file_provider();
            let mut receipts_writer =
                static_file_provider.latest_writer(StaticFileSegment::Receipts).unwrap();
            receipts_writer.increment_block(0).unwrap();
            receipts_writer.commit().unwrap();
        }
        provider.commit().unwrap();

        // insert pre state
        let provider = factory.provider_rw().unwrap();

        let db_tx = provider.tx_ref();
        let acc1 = address!("0x1000000000000000000000000000000000000000");
        let acc2 = address!("0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b");
        let code = hex!("5a465a905090036002900360015500");
        let balance = U256::from(0x3635c9adc5dea00000u128);
        let code_hash = keccak256(code);
        db_tx
            .put::<tables::PlainAccountState>(
                acc1,
                Account { nonce: 0, balance: U256::ZERO, bytecode_hash: Some(code_hash) },
            )
            .unwrap();
        db_tx
            .put::<tables::PlainAccountState>(
                acc2,
                Account { nonce: 0, balance, bytecode_hash: None },
            )
            .unwrap();
        db_tx.put::<tables::Bytecodes>(code_hash, Bytecode::new_raw(code.to_vec().into())).unwrap();
        provider.commit().unwrap();

        // execute

        // If there is a pruning configuration, then it's forced to use the database.
        // This way we test both cases.
        let modes = [None, Some(PruneModes::none())];
        let random_filter = ReceiptsLogPruneConfig(BTreeMap::from([(
            Address::random(),
            PruneMode::Distance(100000),
        )]));

        // Tests node with database and node with static files
        for mut mode in modes {
            let mut provider = factory.database_provider_rw().unwrap();

            if let Some(mode) = &mut mode {
                // Simulating a full node where we write receipts to database
                mode.receipts_log_filter = random_filter.clone();
            }

            let mut execution_stage = stage();
            provider.set_prune_modes(mode.clone().unwrap_or_default());

            let output = execution_stage.execute(&provider, input).unwrap();
            provider.commit().unwrap();

            assert_matches!(output, ExecOutput {
                checkpoint: StageCheckpoint {
                    block_number: 1,
                    stage_checkpoint: Some(StageUnitCheckpoint::Execution(ExecutionCheckpoint {
                        block_range: CheckpointBlockRange {
                            from: 1,
                            to: 1,
                        },
                        progress: EntitiesCheckpoint {
                            processed,
                            total
                        }
                    }))
                },
                done: true
            } if processed == total && total == block.gas_used);

            let provider = factory.provider().unwrap();

            // check post state
            let account1 = address!("0x1000000000000000000000000000000000000000");
            let account1_info =
                Account { balance: U256::ZERO, nonce: 0x00, bytecode_hash: Some(code_hash) };
            let account2 = address!("0x2adc25665018aa1fe0e6bc666dac8fc2697ff9ba");
            let account2_info = Account {
                balance: U256::from(0x1bc16d674ece94bau128),
                nonce: 0x00,
                bytecode_hash: None,
            };
            let account3 = address!("0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b");
            let account3_info = Account {
                balance: U256::from(0x3635c9adc5de996b46u128),
                nonce: 0x01,
                bytecode_hash: None,
            };

            // assert accounts
            assert!(
                matches!(provider.basic_account(&account1), Ok(Some(acc)) if acc == account1_info)
            );
            assert!(
                matches!(provider.basic_account(&account2), Ok(Some(acc)) if acc == account2_info)
            );
            assert!(
                matches!(provider.basic_account(&account3), Ok(Some(acc)) if acc == account3_info)
            );
            // assert storage
            // Get on dupsort would return only first value. This is good enough for this test.
            assert!(matches!(
                provider.tx_ref().get::<tables::PlainStorageState>(account1),
                Ok(Some(entry)) if entry.key == B256::with_last_byte(1) && entry.value == U256::from(2)
            ));

            let mut provider = factory.database_provider_rw().unwrap();
            let mut stage = stage();
            provider.set_prune_modes(mode.unwrap_or_default());

            let _result = stage
                .unwind(
                    &provider,
                    UnwindInput { checkpoint: output.checkpoint, unwind_to: 0, bad_block: None },
                )
                .unwrap();
            provider.commit().unwrap();
        }
    }

    #[tokio::test]
    async fn sanity_execute_unwind() {
        let factory = create_test_provider_factory();
        let provider = factory.provider_rw().unwrap();
        let input = ExecInput { target: Some(1), checkpoint: None };
        let mut genesis_rlp = hex!("f901faf901f5a00000000000000000000000000000000000000000000000000000000000000000a01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347942adc25665018aa1fe0e6bc666dac8fc2697ff9baa045571b40ae66ca7480791bbb2887286e4e4c4b1b298b191c889d6959023a32eda056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421b901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000083020000808502540be400808000a00000000000000000000000000000000000000000000000000000000000000000880000000000000000c0c0").as_slice();
        let genesis = SealedBlock::<Block>::decode(&mut genesis_rlp).unwrap();
        let mut block_rlp = hex!("f90262f901f9a075c371ba45999d87f4542326910a11af515897aebce5265d3f6acd1f1161f82fa01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347942adc25665018aa1fe0e6bc666dac8fc2697ff9baa098f2dcd87c8ae4083e7017a05456c14eea4b1db2032126e27b3b1563d57d7cc0a08151d548273f6683169524b66ca9fe338b9ce42bc3540046c828fd939ae23bcba03f4e5c2ec5b2170b711d97ee755c160457bb58d8daa338e835ec02ae6860bbabb901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000083020000018502540be40082a8798203e800a00000000000000000000000000000000000000000000000000000000000000000880000000000000000f863f861800a8405f5e10094100000000000000000000000000000000000000080801ba07e09e26678ed4fac08a249ebe8ed680bf9051a5e14ad223e4b2b9d26e0208f37a05f6e3f188e3e6eab7d7d3b6568f5eac7d687b08d307d3154ccd8c87b4630509bc0").as_slice();
        let block = SealedBlock::<Block>::decode(&mut block_rlp).unwrap();
        provider.insert_historical_block(genesis.try_recover().unwrap()).unwrap();
        provider.insert_historical_block(block.clone().try_recover().unwrap()).unwrap();
        provider
            .static_file_provider()
            .latest_writer(StaticFileSegment::Headers)
            .unwrap()
            .commit()
            .unwrap();
        {
            let static_file_provider = provider.static_file_provider();
            let mut receipts_writer =
                static_file_provider.latest_writer(StaticFileSegment::Receipts).unwrap();
            receipts_writer.increment_block(0).unwrap();
            receipts_writer.commit().unwrap();
        }
        provider.commit().unwrap();

        // variables
        let code = hex!("5a465a905090036002900360015500");
        let balance = U256::from(0x3635c9adc5dea00000u128);
        let code_hash = keccak256(code);
        // pre state
        let provider = factory.provider_rw().unwrap();

        let db_tx = provider.tx_ref();
        let acc1 = address!("0x1000000000000000000000000000000000000000");
        let acc1_info = Account { nonce: 0, balance: U256::ZERO, bytecode_hash: Some(code_hash) };
        let acc2 = address!("0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b");
        let acc2_info = Account { nonce: 0, balance, bytecode_hash: None };

        db_tx.put::<tables::PlainAccountState>(acc1, acc1_info).unwrap();
        db_tx.put::<tables::PlainAccountState>(acc2, acc2_info).unwrap();
        db_tx.put::<tables::Bytecodes>(code_hash, Bytecode::new_raw(code.to_vec().into())).unwrap();
        provider.commit().unwrap();

        // execute
        let mut provider = factory.database_provider_rw().unwrap();

        // If there is a pruning configuration, then it's forced to use the database.
        // This way we test both cases.
        let modes = [None, Some(PruneModes::none())];
        let random_filter = ReceiptsLogPruneConfig(BTreeMap::from([(
            Address::random(),
            PruneMode::Before(100000),
        )]));

        // Tests node with database and node with static files
        for mut mode in modes {
            if let Some(mode) = &mut mode {
                // Simulating a full node where we write receipts to database
                mode.receipts_log_filter = random_filter.clone();
            }

            // Test Execution
            let mut execution_stage = stage();
            provider.set_prune_modes(mode.clone().unwrap_or_default());

            let result = execution_stage.execute(&provider, input).unwrap();
            provider.commit().unwrap();

            // Test Unwind
            provider = factory.database_provider_rw().unwrap();
            let mut stage = stage();
            provider.set_prune_modes(mode.clone().unwrap_or_default());

            let result = stage
                .unwind(
                    &provider,
                    UnwindInput { checkpoint: result.checkpoint, unwind_to: 0, bad_block: None },
                )
                .unwrap();

            assert_matches!(result, UnwindOutput {
                checkpoint: StageCheckpoint {
                    block_number: 0,
                    stage_checkpoint: Some(StageUnitCheckpoint::Execution(ExecutionCheckpoint {
                        block_range: CheckpointBlockRange {
                            from: 1,
                            to: 1,
                        },
                        progress: EntitiesCheckpoint {
                            processed: 0,
                            total
                        }
                    }))
                }
            } if total == block.gas_used);

            // assert unwind stage
            assert!(matches!(provider.basic_account(&acc1), Ok(Some(acc)) if acc == acc1_info));
            assert!(matches!(provider.basic_account(&acc2), Ok(Some(acc)) if acc == acc2_info));

            let miner_acc = address!("0x2adc25665018aa1fe0e6bc666dac8fc2697ff9ba");
            assert!(matches!(provider.basic_account(&miner_acc), Ok(None)));

            assert!(matches!(provider.receipt(0), Ok(None)));
        }
    }

    #[tokio::test]
    async fn test_selfdestruct() {
        let test_db = TestStageDB::default();
        let provider = test_db.factory.database_provider_rw().unwrap();
        let input = ExecInput { target: Some(1), checkpoint: None };
        let mut genesis_rlp = hex!("f901f8f901f3a00000000000000000000000000000000000000000000000000000000000000000a01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347942adc25665018aa1fe0e6bc666dac8fc2697ff9baa0c9ceb8372c88cb461724d8d3d87e8b933f6fc5f679d4841800e662f4428ffd0da056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421b90100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008302000080830f4240808000a00000000000000000000000000000000000000000000000000000000000000000880000000000000000c0c0").as_slice();
        let genesis = SealedBlock::<Block>::decode(&mut genesis_rlp).unwrap();
        let mut block_rlp = hex!("f9025ff901f7a0c86e8cc0310ae7c531c758678ddbfd16fc51c8cef8cec650b032de9869e8b94fa01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347942adc25665018aa1fe0e6bc666dac8fc2697ff9baa050554882fbbda2c2fd93fdc466db9946ea262a67f7a76cc169e714f105ab583da00967f09ef1dfed20c0eacfaa94d5cd4002eda3242ac47eae68972d07b106d192a0e3c8b47fbfc94667ef4cceb17e5cc21e3b1eebd442cebb27f07562b33836290db90100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000008302000001830f42408238108203e800a00000000000000000000000000000000000000000000000000000000000000000880000000000000000f862f860800a83061a8094095e7baea6a6c7c4c2dfeb977efac326af552d8780801ba072ed817487b84ba367d15d2f039b5fc5f087d0a8882fbdf73e8cb49357e1ce30a0403d800545b8fc544f92ce8124e2255f8c3c6af93f28243a120585d4c4c6a2a3c0").as_slice();
        let block = SealedBlock::<Block>::decode(&mut block_rlp).unwrap();
        provider.insert_historical_block(genesis.try_recover().unwrap()).unwrap();
        provider.insert_historical_block(block.clone().try_recover().unwrap()).unwrap();
        provider
            .static_file_provider()
            .latest_writer(StaticFileSegment::Headers)
            .unwrap()
            .commit()
            .unwrap();
        {
            let static_file_provider = provider.static_file_provider();
            let mut receipts_writer =
                static_file_provider.latest_writer(StaticFileSegment::Receipts).unwrap();
            receipts_writer.increment_block(0).unwrap();
            receipts_writer.commit().unwrap();
        }
        provider.commit().unwrap();

        // variables
        let caller_address = address!("0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b");
        let destroyed_address = address!("0x095e7baea6a6c7c4c2dfeb977efac326af552d87");
        let beneficiary_address = address!("0x2adc25665018aa1fe0e6bc666dac8fc2697ff9ba");

        let code = hex!("73095e7baea6a6c7c4c2dfeb977efac326af552d8731ff00");
        let balance = U256::from(0x0de0b6b3a7640000u64);
        let code_hash = keccak256(code);

        // pre state
        let caller_info = Account { nonce: 0, balance, bytecode_hash: None };
        let destroyed_info =
            Account { nonce: 0, balance: U256::ZERO, bytecode_hash: Some(code_hash) };

        // set account
        let provider = test_db.factory.provider_rw().unwrap();
        provider.tx_ref().put::<tables::PlainAccountState>(caller_address, caller_info).unwrap();
        provider
            .tx_ref()
            .put::<tables::PlainAccountState>(destroyed_address, destroyed_info)
            .unwrap();
        provider
            .tx_ref()
            .put::<tables::Bytecodes>(code_hash, Bytecode::new_raw(code.to_vec().into()))
            .unwrap();
        // set storage to check when account gets destroyed.
        provider
            .tx_ref()
            .put::<tables::PlainStorageState>(
                destroyed_address,
                StorageEntry { key: B256::ZERO, value: U256::ZERO },
            )
            .unwrap();
        provider
            .tx_ref()
            .put::<tables::PlainStorageState>(
                destroyed_address,
                StorageEntry { key: B256::with_last_byte(1), value: U256::from(1u64) },
            )
            .unwrap();

        provider.commit().unwrap();

        // execute
        let provider = test_db.factory.database_provider_rw().unwrap();
        let mut execution_stage = stage();
        let _ = execution_stage.execute(&provider, input).unwrap();
        provider.commit().unwrap();

        // assert unwind stage
        let provider = test_db.factory.database_provider_rw().unwrap();
        assert!(matches!(provider.basic_account(&destroyed_address), Ok(None)));

        assert!(matches!(
            provider.tx_ref().get::<tables::PlainStorageState>(destroyed_address),
            Ok(None)
        ));
        // drops tx so that it returns write privilege to test_tx
        drop(provider);
        let plain_accounts = test_db.table::<tables::PlainAccountState>().unwrap();
        let plain_storage = test_db.table::<tables::PlainStorageState>().unwrap();

        assert_eq!(
            plain_accounts,
            vec![
                (
                    beneficiary_address,
                    Account {
                        nonce: 0,
                        balance: U256::from(0x1bc16d674eca30a0u64),
                        bytecode_hash: None
                    }
                ),
                (
                    caller_address,
                    Account {
                        nonce: 1,
                        balance: U256::from(0xde0b6b3a761cf60u64),
                        bytecode_hash: None
                    }
                )
            ]
        );
        assert!(plain_storage.is_empty());

        let account_changesets = test_db.table::<tables::AccountChangeSets>().unwrap();
        let storage_changesets = test_db.table::<tables::StorageChangeSets>().unwrap();

        assert_eq!(
            account_changesets,
            vec![
                (
                    block.number,
                    AccountBeforeTx { address: destroyed_address, info: Some(destroyed_info) },
                ),
                (block.number, AccountBeforeTx { address: beneficiary_address, info: None }),
                (
                    block.number,
                    AccountBeforeTx { address: caller_address, info: Some(caller_info) }
                ),
            ]
        );

        assert_eq!(
            storage_changesets,
            vec![
                (
                    (block.number, destroyed_address).into(),
                    StorageEntry { key: B256::ZERO, value: U256::ZERO }
                ),
                (
                    (block.number, destroyed_address).into(),
                    StorageEntry { key: B256::with_last_byte(1), value: U256::from(1u64) }
                )
            ]
        );
    }
}
