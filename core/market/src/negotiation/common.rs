use chrono::{DateTime, Utc};
use metrics::counter;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ya_client::model::market::{proposal::Proposal as ClientProposal, reason::Reason, NewProposal};
use ya_client::model::NodeId;
use ya_market_resolver::{match_demand_offer, Match};
use ya_persistence::executor::DbExecutor;
use ya_service_api_web::middleware::Identity;

use crate::config::Config;
use crate::db::model::check_transition;
use crate::db::{
    dao::{
        AgreementDao, AgreementEventsDao, NegotiationEventsDao, ProposalDao, SaveProposalError,
        TakeEventsError,
    },
    model::{
        Agreement, AgreementEvent, AgreementId, AgreementState, AppSessionId, Issuer, MarketEvent,
        Owner, Proposal, ProposalId, ProposalState, SubscriptionId,
    },
};
use crate::matcher::store::SubscriptionStore;
use crate::negotiation::error::{NegotiationError, ProposalValidationError};
use crate::negotiation::{
    error::{
        AgreementError, AgreementEventsError, GetProposalError, MatchValidationError,
        ProposalError, QueryEventsError,
    },
    notifier::NotifierError,
    EventNotifier,
};
use crate::protocol::negotiation::error::{CallerParseError, RejectProposalError};
use crate::protocol::negotiation::messages::ProposalRejected;
use crate::protocol::negotiation::{
    common as protocol_common,
    error::{
        CounterProposalError, RemoteAgreementError, RemoteProposalError, TerminateAgreementError,
    },
    messages::{AgreementTerminated, ProposalReceived},
};
use crate::utils::display::EnableDisplay;

type IsFirst = bool;

#[derive(Clone)]
pub struct CommonBroker {
    pub(super) db: DbExecutor,
    pub(super) store: SubscriptionStore,
    pub(super) negotiation_notifier: EventNotifier<SubscriptionId>,
    pub(super) session_notifier: EventNotifier<AppSessionId>,
    pub(super) agreement_notifier: EventNotifier<AgreementId>,
    pub(super) config: Arc<Config>,
}

impl CommonBroker {
    pub fn new(
        db: DbExecutor,
        store: SubscriptionStore,
        session_notifier: EventNotifier<AppSessionId>,
        config: Arc<Config>,
    ) -> CommonBroker {
        CommonBroker {
            store,
            db: db.clone(),
            negotiation_notifier: EventNotifier::new(),
            session_notifier,
            agreement_notifier: EventNotifier::new(),
            config,
        }
    }

    pub async fn unsubscribe(&self, id: &SubscriptionId) -> Result<(), NegotiationError> {
        self.negotiation_notifier.stop_notifying(id).await;

        // We can ignore error, if removing events failed, because they will be never
        // queried again and don't collide with other subscriptions.
        let _ = self
            .db
            .as_dao::<NegotiationEventsDao>()
            .remove_events(id)
            .await
            .map_err(|e| {
                log::warn!(
                    "Failed to remove events related to subscription [{}]. Error: {}.",
                    id,
                    e
                )
            });

        // TODO: remove all resources related to Proposals
        Ok(())
    }

    pub async fn counter_proposal(
        &self,
        subscription_id: &SubscriptionId,
        prev_proposal_id: &ProposalId,
        proposal: &NewProposal,
        caller_id: &NodeId,
        caller_role: Owner,
    ) -> Result<(Proposal, IsFirst), ProposalError> {
        // Check if subscription is still active.
        // Note that subscription can be unsubscribed, before we get to saving
        // Proposal to database. This seems like race conditions, but there's no
        // danger of data inconsistency. If we won't reject countering Proposal here,
        // it will be sent to Provider and his counter Proposal will be rejected later.

        let prev_proposal = self
            .get_proposal(Some(subscription_id), prev_proposal_id)
            .await?;

        self.validate_proposal(&prev_proposal, caller_id, caller_role)
            .await?;

        let is_first = prev_proposal.body.prev_proposal_id.is_none();
        let new_proposal = prev_proposal.from_client(proposal)?;

        validate_match(&new_proposal, &prev_proposal)?;

        self.db
            .as_dao::<ProposalDao>()
            .save_proposal(&new_proposal)
            .await?;
        Ok((new_proposal, is_first))
    }

    pub async fn reject_proposal(
        &self,
        subs_id: Option<&SubscriptionId>,
        proposal_id: &ProposalId,
        caller_id: &NodeId,
        caller_role: Owner,
        reason: &Option<Reason>,
    ) -> Result<Proposal, RejectProposalError> {
        // Check if subscription is still active.
        // Note that subscription can be unsubscribed, before we get to saving
        // Proposal to database. This seems like race conditions, but there's no
        // danger of data inconsistency. If we won't reject countering Proposal here,
        // it will be sent to Provider and his counter Proposal will be rejected later.

        let proposal = self.get_proposal(subs_id.clone(), proposal_id).await?;

        self.validate_proposal(&proposal, caller_id, caller_role)
            .await?;

        self.db
            .as_dao::<ProposalDao>()
            .change_proposal_state(proposal_id, ProposalState::Rejected)
            .await?;

        log::info!(
            "{:?} [{}] rejected Proposal [{}] {}.",
            caller_role,
            caller_id,
            &proposal_id,
            reason
                .as_ref()
                .map(|r| format!("with reason: {}", r))
                .unwrap_or("without reason".into()),
        );

        Ok(proposal)
    }

    pub async fn query_events(
        &self,
        subscription_id: &SubscriptionId,
        timeout: f32,
        max_events: Option<i32>,
        owner: Owner,
    ) -> Result<Vec<MarketEvent>, QueryEventsError> {
        let mut timeout = Duration::from_secs_f32(timeout.max(0.0));
        let stop_time = Instant::now() + timeout;
        let max_events = max_events.unwrap_or(self.config.events.max_events_default);

        if max_events <= 0 || max_events > self.config.events.max_events_max {
            Err(QueryEventsError::InvalidMaxEvents(
                max_events,
                self.config.events.max_events_max,
            ))?
        }

        let mut notifier = self.negotiation_notifier.listen(subscription_id);
        loop {
            let events = self
                .db
                .as_dao::<NegotiationEventsDao>()
                .take_events(subscription_id, max_events, owner)
                .await?;

            if events.len() > 0 {
                return Ok(events);
            }

            // Solves panic 'supplied instant is later than self'.
            if stop_time < Instant::now() {
                return Ok(vec![]);
            }
            timeout = stop_time - Instant::now();

            if let Err(e) = notifier.wait_for_event_with_timeout(timeout).await {
                return match e {
                    NotifierError::Timeout(_) => Ok(vec![]),
                    NotifierError::ChannelClosed(_) => {
                        Err(QueryEventsError::Internal(e.to_string()))
                    }
                    NotifierError::Unsubscribed(id) => Err(TakeEventsError::NotFound(id).into()),
                };
            }
            // Ok result means, that event with required subscription id was added.
            // We can go to next loop to get this event from db. But still we aren't sure
            // that list won't be empty, because other query_events calls can wait for the same event.
        }
    }

    pub async fn query_agreement_events(
        &self,
        session_id: &AppSessionId,
        timeout: f32,
        max_events: Option<i32>,
        after_timestamp: DateTime<Utc>,
        id: &Identity,
    ) -> Result<Vec<AgreementEvent>, AgreementEventsError> {
        let mut timeout = Duration::from_secs_f32(timeout.max(0.0));
        let stop_time = Instant::now() + timeout;
        let max_events = max_events.unwrap_or(self.config.events.max_events_default);

        if max_events <= 0 || max_events > self.config.events.max_events_max {
            Err(AgreementEventsError::InvalidMaxEvents(
                max_events,
                self.config.events.max_events_max,
            ))?
        }

        let mut agreement_notifier = self.session_notifier.listen(session_id);
        loop {
            let events = self
                .db
                .as_dao::<AgreementEventsDao>()
                .select(
                    &id.identity,
                    session_id,
                    max_events,
                    after_timestamp.naive_utc(),
                )
                .await
                .map_err(|e| AgreementEventsError::Internal(e.to_string()))?;

            if events.len() > 0 {
                counter!("market.agreements.events.queried", events.len() as u64);
                return Ok(events);
            }
            // Solves panic 'supplied instant is later than self'.
            if stop_time < Instant::now() {
                return Ok(vec![]);
            }
            timeout = stop_time - Instant::now();

            if let Err(error) = agreement_notifier
                .wait_for_event_with_timeout(timeout)
                .await
            {
                return match error {
                    NotifierError::Timeout(_) => Ok(vec![]),
                    NotifierError::ChannelClosed(_) => {
                        Err(AgreementEventsError::Internal(error.to_string()))
                    }
                    NotifierError::Unsubscribed(_) => Err(AgreementEventsError::Internal(format!(
                        "Code logic error. Shouldn't get Unsubscribe in Agreement events notifier."
                    ))),
                };
            }
            // Ok result means, that event with required sessionId id was added.
            // We can go to next loop to get this event from db. But still we aren't sure
            // that list won't be empty, because we could get notification for the same appSessionId,
            // but for different identity. Of course we don't return events for other identities,
            // so we will go to sleep again.
        }
    }

    pub async fn get_proposal(
        &self,
        subs_id: Option<&SubscriptionId>,
        id: &ProposalId,
    ) -> Result<Proposal, GetProposalError> {
        Ok(self
            .db
            .as_dao::<ProposalDao>()
            .get_proposal(&id)
            .await
            .map_err(|e| GetProposalError::Internal(id.clone(), subs_id.cloned(), e.to_string()))?
            .filter(|proposal| {
                if subs_id.is_none() {
                    return true;
                }
                let subscription_id = subs_id.unwrap();
                if &proposal.negotiation.subscription_id == subscription_id {
                    return true;
                }
                log::warn!(
                    "Getting Proposal [{}] subscription mismatch; actual: [{}] expected: [{}].",
                    id,
                    proposal.negotiation.subscription_id,
                    subscription_id,
                );
                // We use ProposalNotFound, because we don't want to leak information,
                // that such Proposal exists, but for different subscription_id.
                false
            })
            .ok_or(GetProposalError::NotFound(id.clone(), subs_id.cloned()))?)
    }

    pub async fn get_client_proposal(
        &self,
        subscription_id: Option<&SubscriptionId>,
        id: &ProposalId,
    ) -> Result<ClientProposal, GetProposalError> {
        self.get_proposal(subscription_id, id)
            .await
            .and_then(|proposal| {
                proposal
                    .into_client()
                    .map_err(|e| GetProposalError::Internal(id.clone(), None, e.to_string()))
            })
    }

    // Called locally via REST
    pub async fn terminate_agreement(
        &self,
        id: Identity,
        agreement_id: AgreementId,
        reason: Option<Reason>,
    ) -> Result<(), AgreementError> {
        let dao = self.db.as_dao::<AgreementDao>();
        let agreement = match dao
            .select_by_node(
                agreement_id.clone(),
                id.identity.clone(),
                Utc::now().naive_utc(),
            )
            .await
            .map_err(|e| AgreementError::Get(agreement_id.clone(), e))?
        {
            None => return Err(AgreementError::NotFound(agreement_id)),
            Some(agreement) => agreement,
        };

        // From now on agreement_id is invalid. Use only agreement.id
        // (which has valid owner)
        validate_transition(&agreement, AgreementState::Terminated)?;

        protocol_common::propagate_terminate_agreement(&agreement, reason.clone()).await?;

        let reason_string = CommonBroker::reason2string(&reason);

        dao.terminate(&agreement.id, reason_string, agreement.id.owner())
            .await
            .map_err(|e| AgreementError::UpdateState((&agreement.id).clone(), e))?;
        self.notify_agreement(&agreement).await;

        inc_terminate_metrics(&reason, agreement.id.owner());
        log::info!(
            "{:?} {} terminated Agreement [{}]. Reason: {}",
            agreement.id.owner(),
            &id.display(),
            &agreement.id,
            reason.display(),
        );
        Ok(())
    }

    fn reason2string(reason: &Option<Reason>) -> Option<String> {
        reason.as_ref().map(|reason| {
            serde_json::to_string::<Reason>(reason).unwrap_or(reason.message.to_string())
        })
    }

    // Called remotely via GSB
    pub async fn on_agreement_terminated(
        self,
        msg: AgreementTerminated,
        caller: String,
        caller_role: Owner,
    ) -> Result<(), TerminateAgreementError> {
        let caller_id = CommonBroker::parse_caller(&caller)?;
        Ok(self
            .on_agreement_terminated_inner(msg, caller_id, caller_role)
            .await?)
    }

    async fn on_agreement_terminated_inner(
        self,
        msg: AgreementTerminated,
        caller_id: NodeId,
        caller_role: Owner,
    ) -> Result<(), RemoteAgreementError> {
        let dao = self.db.as_dao::<AgreementDao>();
        let agreement_id = msg.agreement_id.clone();
        let agreement = dao
            .select(&agreement_id, None, Utc::now().naive_utc())
            .await
            .map_err(|_e| RemoteAgreementError::NotFound(agreement_id.clone()))?
            .ok_or(RemoteAgreementError::NotFound(agreement_id.clone()))?;

        let auth_id = match caller_role {
            Owner::Provider => agreement.provider_id,
            Owner::Requestor => agreement.requestor_id,
        };

        if auth_id != caller_id {
            // Don't reveal, that we know this Agreement id.
            Err(RemoteAgreementError::NotFound(agreement_id.clone()))?
        }

        let reason_string = CommonBroker::reason2string(&msg.reason);

        dao.terminate(&agreement_id, reason_string, caller_role)
            .await
            .map_err(|e| {
                log::warn!(
                    "Couldn't terminate agreement. id: {}, e: {}",
                    agreement_id,
                    e
                );
                RemoteAgreementError::InternalError(agreement_id.clone())
            })?;

        self.notify_agreement(&agreement).await;

        inc_terminate_metrics(&msg.reason, agreement.id.owner());
        log::info!(
            "Received terminate Agreement [{}] from [{}]. Reason: {}",
            &agreement_id,
            &caller_id,
            msg.reason.display(),
        );
        Ok(())
    }

    // TODO: We need more elegant solution than this. This function still returns
    //  CounterProposalError, which should be hidden in negotiation API and implementations
    //  of handlers should return RemoteProposalError.
    pub async fn on_proposal_received(
        self,
        msg: ProposalReceived,
        caller: String,
        caller_role: Owner,
    ) -> Result<(), CounterProposalError> {
        let proposal_id = msg.proposal.proposal_id.clone();
        let caller_id = CommonBroker::parse_caller(&caller)?;
        self.proposal_received(msg, caller_id, caller_role)
            .await
            .map_err(|e| CounterProposalError::Remote(e, proposal_id))
    }

    pub async fn proposal_received(
        self,
        msg: ProposalReceived,
        caller_id: NodeId,
        caller_role: Owner,
    ) -> Result<(), RemoteProposalError> {
        // Check if countered Proposal exists.
        let prev_proposal = self
            .get_proposal(None, &msg.prev_proposal_id)
            .await
            .map_err(|_e| RemoteProposalError::NotFound(msg.prev_proposal_id.clone()))?;
        let proposal = prev_proposal.from_draft(msg.proposal);
        proposal.validate_id()?;

        self.validate_proposal(&prev_proposal, &caller_id, caller_role)
            .await?;
        validate_match(&proposal, &prev_proposal)?;

        self.db
            .as_dao::<ProposalDao>()
            .save_proposal(&proposal)
            .await
            .map_err(|e| match e {
                SaveProposalError::AlreadyCountered(id) => {
                    RemoteProposalError::AlreadyCountered(id)
                }
                _ => {
                    // TODO: Don't leak our database error, but send meaningful message as response.
                    let msg = format!("Failed saving Proposal [{}]: {}", proposal.body.id, e);
                    log::warn!("{}", msg);
                    ProposalValidationError::Internal(msg).into()
                }
            })?;

        // Create Proposal Event and add it to queue (database).
        // TODO: If creating Proposal succeeds, but event can't be added, provider
        // TODO: will never answer to this Proposal. Solve problem when Event API will be available.
        let subscription_id = proposal.negotiation.subscription_id.clone();
        self.db
            .as_dao::<NegotiationEventsDao>()
            .add_proposal_event(&proposal, caller_role.swap())
            .await
            .map_err(|e| {
                // TODO: Don't leak our database error, but send meaningful message as response.
                let msg = format!("Failed adding Proposal [{}] Event: {}", proposal.body.id, e);
                log::warn!("{}", msg);
                ProposalValidationError::Internal(msg)
            })?;

        // Send channel message to wake all query_events waiting for proposals.
        self.negotiation_notifier.notify(&subscription_id).await;

        match caller_role {
            Owner::Requestor => counter!("market.proposals.requestor.received", 1),
            Owner::Provider => counter!("market.proposals.provider.received", 1),
        };
        log::info!(
            "Received counter Proposal [{}] for Proposal [{}] from [{}].",
            &proposal.body.id,
            &msg.prev_proposal_id,
            &caller_id
        );
        Ok(())
    }

    pub async fn on_proposal_rejected(
        self,
        msg: ProposalRejected,
        caller: String,
        caller_role: Owner,
    ) -> Result<(), RejectProposalError> {
        let caller_id = CommonBroker::parse_caller(&caller)?;
        self.proposal_rejected(msg, caller_id, caller_role).await
    }

    pub async fn proposal_rejected(
        self,
        msg: ProposalRejected,
        caller_id: NodeId,
        caller_role: Owner,
    ) -> Result<(), RejectProposalError> {
        let proposal = CommonBroker::reject_proposal(
            &self,
            None,
            &msg.proposal_id,
            &caller_id,
            caller_role,
            &msg.reason,
        )
        .await?;

        // Create Proposal Event and add it to queue (database).
        // TODO: If creating Proposal succeeds, but event can't be added, provider
        // TODO: will never answer to this Proposal. Solve problem when Event API will be available.
        let subscription_id = proposal.negotiation.subscription_id.clone();
        let reason = CommonBroker::reason2string(&msg.reason);
        self.db
            .as_dao::<NegotiationEventsDao>()
            .add_proposal_rejected_event(&proposal, reason)
            .await
            .map_err(|e| {
                // TODO: Don't leak our database error, but send meaningful message as response.
                let msg = format!(
                    "Failed adding Proposal [{}] Rejected Event: {}",
                    msg.proposal_id, e
                );
                log::warn!("{}", msg);
                ProposalValidationError::Internal(msg)
            })?;

        // Send channel message to wake all query_events waiting for proposals.
        self.negotiation_notifier.notify(&subscription_id).await;

        match caller_role {
            Owner::Provider => counter!("market.proposals.requestor.rejected.by-them", 1),
            Owner::Requestor => counter!("market.proposals.provider.rejected.by-them", 1),
        };

        Ok(())
    }

    pub(crate) fn parse_caller(caller: &str) -> Result<NodeId, CallerParseError> {
        NodeId::from_str(caller).map_err(|e| CallerParseError {
            caller: caller.to_string(),
            e: e.to_string(),
        })
    }

    pub async fn validate_proposal(
        &self,
        proposal: &Proposal,
        caller_id: &NodeId,
        caller_role: Owner,
    ) -> Result<(), ProposalValidationError> {
        if match caller_role {
            Owner::Provider => &proposal.negotiation.provider_id != caller_id,
            Owner::Requestor => &proposal.negotiation.requestor_id != caller_id,
        } {
            ProposalValidationError::Unauthorized(proposal.body.id.clone(), caller_id.clone());
        }

        if proposal.body.issuer == Issuer::Us && proposal.body.id.owner() == caller_role {
            let e = ProposalValidationError::OwnProposal(prev_proposal.body.id.clone());
            log::warn!("{}", e);
            counter!("market.proposals.self-reaction-attempt", 1);
            Err(e)?;
        }

        // check Offer
        self.store.get_offer(&proposal.negotiation.offer_id).await?;

        // On Requestor side we have both Offer and Demand, but Provider has only Offers.
        if proposal.body.id.owner() == Owner::Requestor {
            self.store
                .get_demand(&proposal.negotiation.demand_id)
                .await?;
        }

        Ok(())
    }

    pub async fn notify_agreement(&self, agreement: &Agreement) {
        let session_notifier = &self.session_notifier;

        // Notify everyone waiting on Agreement events endpoint.
        if let Some(_) = &agreement.session_id {
            session_notifier.notify(&agreement.session_id.clone()).await;
        }
        // Even if session_id was not None, we want to notify everyone else,
        // that waits without specifying session_id.
        session_notifier.notify(&None).await;

        // This notifies wait_for_agreement endpoint.
        self.agreement_notifier.notify(&agreement.id).await;
    }
}

pub fn validate_match(
    new_proposal: &Proposal,
    prev_proposal: &Proposal,
) -> Result<(), MatchValidationError> {
    match match_demand_offer(
        &new_proposal.body.properties,
        &new_proposal.body.constraints,
        &prev_proposal.body.properties,
        &prev_proposal.body.constraints,
    )
    .map_err(|e| MatchValidationError::MatchingFailed {
        new: new_proposal.body.id.clone(),
        prev: prev_proposal.body.id.clone(),
        error: e.to_string(),
    })? {
        Match::Yes => Ok(()),
        _ => {
            return Err(MatchValidationError::NotMatching {
                new: new_proposal.body.id.clone(),
                prev: prev_proposal.body.id.clone(),
            })
        }
    }
}

pub fn validate_transition(
    agreement: &Agreement,
    state: AgreementState,
) -> Result<(), AgreementError> {
    check_transition(agreement.state, state)
        .map_err(|e| AgreementError::UpdateState(agreement.id.clone(), e))
}

fn get_reason_code(reason: &Option<Reason>, key: &str) -> Option<String> {
    reason
        .as_ref()
        .map(|reason| {
            reason
                .extra
                .get(key)
                .map(|json| json.as_str().map(|code| code.to_string()))
        })
        .flatten()
        .flatten()
}

/// This function extract from Reason additional information about termination reason
/// and increments metric counter. Note that Reason isn't required to have any fields
/// despite 'message'.
pub fn inc_terminate_metrics(reason: &Option<Reason>, owner: Owner) {
    match owner {
        Owner::Provider => counter!("market.agreements.provider.terminated", 1),
        Owner::Requestor => counter!("market.agreements.requestor.terminated", 1),
    };

    let p_code = get_reason_code(reason, "golem.provider.code");
    let r_code = get_reason_code(reason, "golem.requestor.code");

    let reason_code = r_code.xor(p_code).unwrap_or("NotSpecified".to_string());
    match owner {
        Owner::Provider => {
            counter!("market.agreements.provider.terminated.reason", 1, "reason" => reason_code)
        }
        Owner::Requestor => {
            counter!("market.agreements.requestor.terminated.reason", 1, "reason" => reason_code)
        }
    };
}
