//! Validate and admit one linear owned-argument transaction for data and stream exports.
use super::*;
use crate::worker::{
    StreamCall,
    arguments::{Arguments, Resource as ArgumentResource},
};

impl Environment {
    /// The same transaction serves ordinary results and published byte-stream heads. The
    /// worker acknowledges canonical acceptance; until then installed foreign slots roll back.
    pub(in crate::bindings) async fn invoke_arguments(
        &self,
        context: Context,
        slot: u32,
        receiver: Option<u32>,
        mut payload: Payload,
        stream: Option<StreamCall>,
    ) -> Result<Value> {
        let moves = payload.take_arguments();
        let mut arguments = payload.into_data()?;
        let mut moves: Vec<_> = moves.into_iter().map(Some).collect();
        let mut carried = Vec::new();
        let mut retained = Vec::new();
        let mut delivery: Option<Delivery> = None;
        self.contract
            .map_owned_arguments(slot, &mut arguments, |identity, index| {
                let expected = nominal(identity)
                    .map_err(|error| tembra_contracts::Error(error.to_string()))?;
                let moved = moves
                    .get_mut(index as usize)
                    .and_then(Option::take)
                    .ok_or_else(|| {
                        tembra_contracts::Error(
                            "an owned argument redeems each attachment exactly once".into(),
                        )
                    })?;
                if moved.nominal() != &expected {
                    return Err(tembra_contracts::Error(
                        "the owned argument has another nominal type".into(),
                    ));
                }
                let local = moved
                    .with_backend::<GuestInstance, _>(|backend| {
                        backend
                            .issuer
                            .upgrade()
                            .is_some_and(|issuer| issuer.session.same_revision(&self.session))
                            .then_some(ArgumentResource::Issued {
                                environment: backend.environment,
                                instance: backend.instance,
                            })
                    })
                    .flatten();
                let resource = match local {
                    Some(resource) => {
                        retained.push(moved);
                        resource
                    }
                    None => {
                        let binding = self
                            .argument_import(&expected)
                            .map_err(|error| tembra_contracts::Error(error.to_string()))?;
                        let (slot, installed) = self
                            .install(moved.receive(), binding)
                            .map_err(|error| tembra_contracts::Error(error.to_string()))?;
                        match &mut delivery {
                            Some(delivery) => delivery.slots.extend(installed.into_slots()),
                            None => delivery = Some(installed),
                        }
                        ArgumentResource::Imported { slot }
                    }
                };
                let index = u32::try_from(carried.len())
                    .map_err(|error| tembra_contracts::Error(error.to_string()))?;
                carried.push(resource);
                Ok(index)
            })
            .map_err(failure)?;
        if moves.iter().any(Option::is_some) {
            return Err(misuse("the arguments carry an unused resource attachment"));
        }
        let args = self
            .contract
            .to_resource_wire(slot, true, arguments)
            .map_err(failure)?;
        let context = context.enter_execution()?;
        let cancellation = context.signal();
        let packet = GuestCommand::Invoke {
            environment: self.id,
            function: slot,
            args: &args,
            receiver,
        };
        let value = if carried.is_empty() {
            self.execute(
                Scope::Call(context),
                Some(cancellation),
                packet,
                stream.map(Carrier::Stream).unwrap_or_default(),
            )
            .await?
        } else {
            let (accepted, mut admission) = tokio::sync::oneshot::channel();
            let execution = self.execute(
                Scope::Call(context),
                Some(cancellation),
                packet,
                Carrier::Arguments {
                    arguments: Arguments {
                        resources: carried,
                        accepted,
                    },
                    stream,
                },
            );
            let mut execution = std::pin::pin!(execution);
            let result = match futures::future::select(execution.as_mut(), &mut admission).await {
                futures::future::Either::Left((result, _)) => {
                    // The call may accept and complete in the same poll.
                    if admission.try_recv().is_ok()
                        && let Some(delivery) = delivery.take()
                    {
                        delivery.commit();
                    }
                    result
                }
                futures::future::Either::Right((accepted, _)) => {
                    if accepted.is_ok()
                        && let Some(delivery) = delivery.take()
                    {
                        delivery.commit();
                    }
                    execution.await
                }
            };
            result?
        };
        drop(retained);
        Ok(value)
    }
}
