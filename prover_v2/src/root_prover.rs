use crate::contexts::ProveContext;
use crate::{get_prover, NetworkProve, Segment, KEY_CACHE, PROGRAM_CACHE};
use common::file;
use zkm_core_machine::utils::trace_checkpoint;
use zkm_prover::CoreSC;
use zkm_stark::{MachineProver, MachineProvingKey, StarkGenericConfig};

#[derive(Default)]
pub struct RootProver {}

impl RootProver {
    pub fn prove(&self, ctx: &ProveContext) -> anyhow::Result<Vec<u8>> {
        let now = std::time::Instant::now();
        let segment: Segment = {
            let mut retries = 0;
            const MAX_RETRIES: usize = 10;

            loop {
                let result = std::fs::read(&ctx.segment)
                    .and_then(|segment| {
                        zstd::stream::decode_all(&*segment)
                            .map_err(|e| std::io::Error::other(format!("zstd decode failed: {e}")))
                    })
                    .and_then(|decoded| {
                        bincode::deserialize::<Segment>(&decoded).map_err(|e| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!("deserialize failed: {e}"),
                            )
                        })
                    });

                match result {
                    Ok(r) => break r,
                    Err(e) => {
                        if retries >= MAX_RETRIES {
                            return Err(anyhow::anyhow!(
                                "Segment read/decode failed after {} retries: {}",
                                MAX_RETRIES,
                                e
                            ));
                        }
                        tracing::warn!("Segment {:?} error: {}, retrying...", ctx.segment, e);
                        retries += 1;
                        std::thread::sleep(std::time::Duration::from_millis(300));
                    }
                }
            }
        };
        tracing::info!("read segment time: {:?}", now.elapsed());

        let network_prove = NetworkProve::new(ctx.seg_size);
        let opts = network_prove.opts.core_opts;
        let prover = get_prover();

        let mut record = match segment {
            Segment::State(state) => {
                let program_slot = PROGRAM_CACHE.lock().get_or_init_slot(&ctx.program_id);
                let program = program_slot.get_or_try_init(|| {
                    tracing::info!("No program in cache, generate new program");
                    let elf = file::new(&ctx.elf_path).read()?;
                    prover
                        .get_program(&elf)
                        .map_err(|e| anyhow::Error::msg(e.to_string()))
                })?;
                let public_values = state.public_values;
                let (records, _) = tracing::debug_span!("trace checkpoint").in_scope(|| {
                    trace_checkpoint::<CoreSC>(
                        program.clone(),
                        state.state,
                        opts,
                        prover.core_shape_config.as_ref(),
                    )
                });
                let mut record = records.into_iter().next().unwrap();
                let _ = record.defer();
                record.public_values = public_values;
                record
            }
            Segment::Record(record) => *record,
        };

        let now = std::time::Instant::now();
        let key_slot = KEY_CACHE.lock().get_or_init_slot(&ctx.program_id);
        let (pk, _vk) = key_slot.get_or_try_init(|| {
            Ok::<_, anyhow::Error>(prover.core_prover.setup(&record.program))
        })?;
        tracing::info!("setup time: {:?}", now.elapsed());
        let now = std::time::Instant::now();
        prover.core_prover.machine().generate_dependencies(
            std::slice::from_mut(&mut record),
            &opts,
            None,
        );
        tracing::info!("generate dependencies time: {:?}", now.elapsed());

        // Fix the shape of the record.
        let now = std::time::Instant::now();
        if let Some(shape_config) = &prover.core_shape_config {
            shape_config.fix_shape(&mut record)?;
        }
        tracing::info!("fix shape time: {:?}", now.elapsed());
        let now = std::time::Instant::now();
        let main_trace = prover.core_prover.generate_traces(&record);
        tracing::info!("generate traces time: {:?}", now.elapsed());

        let mut challenger = prover.core_prover.config().challenger();
        pk.observe_into(&mut challenger);
        let now = std::time::Instant::now();
        let main_data = prover.core_prover.commit(&record, main_trace);
        tracing::info!("commit time: {:?}", now.elapsed());
        let now = std::time::Instant::now();
        let proof = prover.core_prover.open(pk, main_data, &mut challenger)?;
        tracing::info!("open time: {:?}", now.elapsed());

        Ok(bincode::serialize(&proof)?)
    }
}
