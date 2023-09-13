use crate::msg::{FinalizeAnswer, QueryAnswer, QueryMsg, ResponseStatus};
use crate::querier::query_staking_balance;
use crate::state::{
    read_vote, store_vote, StoredPollConfig, StoredRevealConfig, Vote, CONFIG_KEY, METADATA_KEY,
    NUM_OF_VOTERS_KEY, OWNER_KEY, REVEAL_CONFIG, STAKING_POOL_KEY, TALLY_KEY,
};
use cosmwasm_std::{
    log, to_binary, Api, Binary, CosmosMsg, Env, Extern, HandleResponse, HumanAddr, InitResponse,
    Querier, StdError, StdResult, Storage, Uint128, WasmMsg,
};
use scrt_finance::secret_vote_types::{PollHandleMsg, PollInitMsg, PollMetadata};
use scrt_finance::types::SecretContract;
use secret_toolkit::snip20;
use secret_toolkit::snip20::{balance_query, Balance};
use secret_toolkit::storage::{TypedStore, TypedStoreMut};
use sha2::{Digest, Sha256};
use std::mem::size_of;

pub fn init<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    msg: PollInitMsg,
) -> StdResult<InitResponse> {
    let owner = env.message.sender;
    TypedStoreMut::attach(&mut deps.storage).store(OWNER_KEY, &owner)?; // This is in fact the factory contract
    TypedStoreMut::attach(&mut deps.storage).store(STAKING_POOL_KEY, &msg.staking_pool)?;

    if msg.choices.len() < 2 {
        return Err(StdError::generic_err(
            "you have to provide at least two choices",
        ));
    }

    // Sanity checks to prevent starting a new poll by mistake
    if msg.metadata.title.len() < 2 {
        return Err(StdError::generic_err(
            "poll title must be at least 2 characters long",
        ));
    }
    if msg.metadata.description.len() < 10 {
        return Err(StdError::generic_err(
            "poll description must be at least 10 characters long",
        ));
    }
    if msg.metadata.author_alias.len() < 3 {
        return Err(StdError::generic_err(
            "poll author alias must be at least 3 characters long",
        ));
    }
    TypedStoreMut::attach(&mut deps.storage).store(METADATA_KEY, &msg.metadata)?;

    let tally: Vec<u128> = vec![0; msg.choices.len()];
    TypedStoreMut::attach(&mut deps.storage).store(TALLY_KEY, &tally)?;

    let ending = env.block.time + msg.config.duration;
    TypedStoreMut::attach(&mut deps.storage).store(
        CONFIG_KEY,
        &StoredPollConfig {
            end_timestamp: ending,
            quorum: msg.config.quorum,
            min_threshold: msg.config.min_threshold,
            choices: msg.choices,
            finalized: false,
            valid: false,
            rolling_hash: [0u8; 32],
        },
    )?;

    TypedStoreMut::attach(&mut deps.storage).store(NUM_OF_VOTERS_KEY, &(0_u64))?;
    TypedStoreMut::attach(&mut deps.storage).store(
        REVEAL_CONFIG,
        &StoredRevealConfig {
            committee: msg.reveal_com,
            num_revealed: 0,
            revealed: vec![],
        },
    )?;

    let mut messages = vec![];
    if let Some(init_hook) = msg.init_hook {
        messages.push(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: init_hook.contract_addr,
            callback_code_hash: init_hook.code_hash,
            msg: init_hook.msg,
            send: vec![],
        }));
    }

    Ok(InitResponse {
        messages,
        log: vec![],
    })
}

pub fn handle<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    msg: PollHandleMsg,
) -> StdResult<HandleResponse> {
    match msg {
        PollHandleMsg::Vote {
            choice,
            staking_pool_viewing_key,
            salt,
        } => vote(deps, env, choice, staking_pool_viewing_key, salt),
        PollHandleMsg::UpdateVotingPower { voter, new_power } => {
            update_voting_power(deps, env, voter, new_power.u128())
        }
        PollHandleMsg::Finalize { rolling_hash } => finalize(deps, env, rolling_hash),
    }
}

pub fn query<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
    msg: QueryMsg,
) -> StdResult<Binary> {
    match msg {
        QueryMsg::Choices {} => query_choices(deps),
        QueryMsg::HasVoted { voter } => query_has_voted(deps, voter),
        QueryMsg::Tally {} => query_tally(deps),
        QueryMsg::Vote { voter, key } => query_vote(deps, voter, key),
        QueryMsg::NumberOfVoters {} => query_num_of_voters(deps),
        QueryMsg::VoteInfo {} => query_vote_info(deps),
        QueryMsg::RevealCommittee {} => query_reveal_com(deps),
        QueryMsg::Revealed {} => query_revealed(deps),
        QueryMsg::RollingHash {} => query_rolling_hash(deps),
    }
}

// Handle

pub fn vote<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    choice: u8,
    key: String,
    salt: String,
) -> StdResult<HandleResponse> {
    let mut config = TypedStore::attach(&deps.storage).load(CONFIG_KEY)?;
    require_vote_ongoing(&env, &config)?;

    let staking_pool: SecretContract = TypedStore::attach(&deps.storage).load(STAKING_POOL_KEY)?;
    let voting_power = snip20::balance_query(
        &deps.querier,
        env.message.sender.clone(),
        key,
        256,
        staking_pool.contract_hash,
        staking_pool.address,
    )?
    .amount
    .u128();

    let prev_vote = read_vote(deps, &env.message.sender).ok();
    update_vote(
        deps,
        &env.message.sender,
        prev_vote,
        Vote {
            choice,
            voting_power,
        },
    )?;

    let new_hash = roll_hash(
        config.rolling_hash,
        &env.message.sender,
        Vote {
            choice,
            voting_power,
        },
        salt,
    );
    config.rolling_hash = new_hash;
    TypedStoreMut::attach(&mut deps.storage).store(CONFIG_KEY, &config)?;

    Ok(HandleResponse {
        messages: vec![],
        log: vec![log("voted", env.message.sender.to_string())],
        data: Some(to_binary(&ResponseStatus::Success)?),
    })
}

pub fn update_voting_power<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    voter: HumanAddr,
    new_power: u128,
) -> StdResult<HandleResponse> {
    let config = TypedStore::attach(&deps.storage).load(CONFIG_KEY)?;
    require_vote_ongoing(&env, &config)?;

    let owner: HumanAddr = TypedStore::attach(&deps.storage).load(OWNER_KEY)?;
    if env.message.sender != owner {
        return Err(StdError::unauthorized());
    }

    let mut logs = vec![];
    if let Ok(prev_vote) = read_vote(deps, &voter) {
        update_vote(
            deps,
            &voter,
            Some(prev_vote.clone()),
            Vote {
                choice: prev_vote.choice,
                voting_power: new_power,
            },
        )?;

        logs.push(log("voting_power_updated", voter.to_string()));
    }

    Ok(HandleResponse {
        messages: vec![],
        log: logs,
        data: Some(to_binary(&ResponseStatus::Success)?),
    })
}

pub fn finalize<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    rolling_hash: String,
) -> StdResult<HandleResponse> {
    let mut config: StoredPollConfig = TypedStoreMut::attach(&mut deps.storage).load(CONFIG_KEY)?;
    if env.block.time < config.end_timestamp {
        return Err(StdError::generic_err("vote has not ended yet"));
    }

    if hex::encode(&config.rolling_hash) != rolling_hash {
        return Err(StdError::generic_err("incorrect rolling hash"));
    }

    let mut reveal_conf_store = TypedStoreMut::attach(&mut deps.storage);
    let mut reveal_conf: StoredRevealConfig = reveal_conf_store.load(REVEAL_CONFIG)?;
    if !reveal_conf
        .committee
        .revealers
        .contains(&env.message.sender)
    {
        return Err(StdError::unauthorized());
    }

    if reveal_conf.revealed.contains(&env.message.sender) {
        return Err(StdError::generic_err("already finalized the vote"));
    }

    reveal_conf.revealed.push(env.message.sender);
    reveal_conf.num_revealed += 1;
    reveal_conf_store.store(REVEAL_CONFIG, &reveal_conf)?;

    if reveal_conf.num_revealed > reveal_conf.committee.n {
        let tally: Vec<u128> = TypedStore::attach(&deps.storage).load(TALLY_KEY)?; // Already revealed
        return Ok(HandleResponse {
            messages: vec![],
            log: vec![],
            data: Some(to_binary(&FinalizeAnswer {
                finalized: config.finalized,
                valid: Some(config.valid),
                choices: Some(config.choices),
                tally: Some(tally.iter().map(|c| Uint128(*c)).collect()),
            })?),
        });
    } else if reveal_conf.num_revealed < reveal_conf.committee.n {
        return Ok(HandleResponse {
            messages: vec![],
            log: vec![],
            data: Some(to_binary(&FinalizeAnswer {
                finalized: false,
                valid: None,
                choices: None,
                tally: None,
            })?),
        });
    }

    config.finalized = true;

    let tally: Vec<u128> = TypedStore::attach(&deps.storage).load(TALLY_KEY)?;

    // Validation tests
    let sefi_balance = query_staking_balance(deps)?;
    let total_vote_count: u128 = tally.iter().sum();
    let participation = 100 * total_vote_count / sefi_balance; // This should give a percentage integer X/100%
    if participation > config.quorum as u128 {
        config.valid = true;
    }
    if let Some(winning_choice) = tally.iter().max() {
        config.valid = config.valid && (*winning_choice > config.min_threshold as u128)
    } else {
        return Err(StdError::generic_err("storage is corrupted")); // iter().max() returns `None` only when the Vec is empty
    }

    TypedStoreMut::attach(&mut deps.storage).store(CONFIG_KEY, &config)?;
    Ok(HandleResponse {
        messages: vec![],
        log: vec![],
        data: Some(to_binary(&FinalizeAnswer {
            finalized: config.finalized,
            valid: Some(config.valid),
            choices: Some(config.choices),
            tally: Some(tally.iter().map(|c| Uint128(*c)).collect()),
        })?),
    })
}

// Query

pub fn query_choices<S: Storage, A: Api, Q: Querier>(deps: &Extern<S, A, Q>) -> StdResult<Binary> {
    let config: StoredPollConfig = TypedStore::attach(&deps.storage).load(CONFIG_KEY)?;
    Ok(to_binary(&QueryAnswer::Choices {
        choices: config.choices,
    })?)
}

pub fn query_vote_info<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
) -> StdResult<Binary> {
    let metadata: PollMetadata = TypedStore::attach(&deps.storage).load(METADATA_KEY)?;
    let config: StoredPollConfig = TypedStore::attach(&deps.storage).load(CONFIG_KEY)?;
    let reveal_conf: StoredRevealConfig = TypedStore::attach(&deps.storage).load(REVEAL_CONFIG)?;
    Ok(to_binary(&QueryAnswer::VoteInfo {
        metadata,
        config,
        reveal_com: reveal_conf.committee,
    })?)
}

pub fn query_has_voted<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
    voter: HumanAddr,
) -> StdResult<Binary> {
    let has_voted = read_vote(deps, &voter).is_ok();
    Ok(to_binary(&QueryAnswer::HasVoted { has_voted })?)
}

pub fn query_tally<S: Storage, A: Api, Q: Querier>(deps: &Extern<S, A, Q>) -> StdResult<Binary> {
    let config = TypedStore::attach(&deps.storage).load(CONFIG_KEY)?;
    require_vote_finalized_and_valid(&config)?; // Hopefully this provide a good enough anonymity set

    let tally: Vec<u128> = TypedStore::attach(&deps.storage).load(TALLY_KEY)?;
    let formatted_tally: Vec<Uint128> = tally.iter().map(|c| Uint128(*c)).collect();
    let config: StoredPollConfig = TypedStore::attach(&deps.storage).load(CONFIG_KEY)?;
    Ok(to_binary(&QueryAnswer::Tally {
        choices: config.choices,
        tally: formatted_tally,
    })?)
}

pub fn query_vote<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
    voter: HumanAddr,
    key: String,
) -> StdResult<Binary> {
    let staking_pool: SecretContract = TypedStore::attach(&deps.storage).load(STAKING_POOL_KEY)?;
    let _balance: Balance = balance_query(
        &deps.querier,
        voter.clone(),
        key,
        256,
        staking_pool.contract_hash,
        staking_pool.address,
    )?; // Balance doesn't matter, we're just verifying the viewing key

    let vote: Vote = TypedStore::attach(&deps.storage).load(voter.0.as_bytes())?;
    Ok(to_binary(&QueryAnswer::Vote {
        choice: vote.choice,
        voting_power: Uint128(vote.voting_power),
    })?)
}

pub fn query_num_of_voters<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
) -> StdResult<Binary> {
    let num_of_voters: u64 = TypedStore::attach(&deps.storage).load(NUM_OF_VOTERS_KEY)?;

    Ok(to_binary(&QueryAnswer::NumberOfVoters {
        count: num_of_voters,
    })?)
}

pub fn query_reveal_com<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
) -> StdResult<Binary> {
    let reveal_config: StoredRevealConfig =
        TypedStore::attach(&deps.storage).load(REVEAL_CONFIG)?;

    Ok(to_binary(&QueryAnswer::RevealCommittee {
        committee: reveal_config.committee,
    })?)
}

pub fn query_revealed<S: Storage, A: Api, Q: Querier>(deps: &Extern<S, A, Q>) -> StdResult<Binary> {
    let reveal_config: StoredRevealConfig =
        TypedStore::attach(&deps.storage).load(REVEAL_CONFIG)?;

    Ok(to_binary(&QueryAnswer::Revealed {
        required: reveal_config.committee.n,
        num_revealed: reveal_config.num_revealed,
        revealed: reveal_config.revealed,
    })?)
}

pub fn query_rolling_hash<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
) -> StdResult<Binary> {
    let config: StoredPollConfig = TypedStore::attach(&deps.storage).load(CONFIG_KEY)?;
    let hash = config.rolling_hash;

    Ok(to_binary(&QueryAnswer::RollingHash {
        hash: hex::encode(&hash),
    })?)
}

// Helper functions

fn update_vote<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    voter: &HumanAddr,
    previous_vote: Option<Vote>,
    new_vote: Vote,
) -> StdResult<()> {
    let mut tally: Vec<u128> = TypedStoreMut::attach(&mut deps.storage).load(TALLY_KEY)?;

    if let Some(previous_vote) = previous_vote {
        if let Some(choice_tally) = tally.get_mut(previous_vote.choice as usize) {
            *choice_tally -= previous_vote.voting_power; // Can't underflow, `choice_tally` >= `old_vote.voting_power`
        } else {
            // Shouldn't really happen since user already voted, but just in case
            return Err(StdError::generic_err(format!(
                "previous choice {} does not exist in this poll",
                previous_vote.choice
            )));
        }
    } else {
        // If it's a new vote - increment the number of voters
        let mut voters_store = TypedStoreMut::attach(&mut deps.storage);
        let num_of_voters: u64 = voters_store.load(NUM_OF_VOTERS_KEY)?;
        voters_store.store(NUM_OF_VOTERS_KEY, &(num_of_voters + 1))?;
    }

    if let Some(choice_tally) = tally.get_mut(new_vote.choice as usize) {
        *choice_tally += new_vote.voting_power; // Can't overflow, `choice_tally` <= `gov_token.total_supply()`
    } else {
        return Err(StdError::generic_err(format!(
            "choice {} does not exist in this poll",
            new_vote.choice
        )));
    }

    TypedStoreMut::attach(&mut deps.storage).store(TALLY_KEY, &tally)?;
    store_vote(deps, voter, new_vote.choice, new_vote.voting_power)?; // This also discards the old vote

    Ok(())
}

fn roll_hash(hash: [u8; 32], voter: &HumanAddr, vote: Vote, salt: String) -> [u8; 32] {
    let mut extended = Vec::with_capacity(
        hash.len() + voter.0.len() + size_of::<u8>() + size_of::<u128>() + salt.len(),
    );
    extended.extend_from_slice(voter.0.as_bytes());
    extended.extend_from_slice(&vote.choice.to_le_bytes());
    extended.extend_from_slice(&vote.voting_power.to_le_bytes());
    extended.extend_from_slice(&salt.as_bytes());

    Sha256::digest(&extended).into()
}

fn require_vote_ongoing(env: &Env, config: &StoredPollConfig) -> StdResult<()> {
    if config.end_timestamp < env.block.time {
        return Err(StdError::generic_err("vote has ended"));
    }

    Ok(())
}

fn require_vote_finalized_and_valid(config: &StoredPollConfig) -> StdResult<()> {
    if !config.finalized {
        return Err(StdError::generic_err("vote hasn't been finalized yet"));
    } else if !config.valid {
        return Err(StdError::generic_err("vote hasn't passed quorum"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmwasm_std::testing::{
        mock_dependencies, MockApi, MockQuerier, MockStorage, MOCK_CONTRACT_ADDR,
    };
    use cosmwasm_std::{coins, from_binary, BlockInfo, Coin, ContractInfo, MessageInfo, StdError};
    use scrt_finance::secret_vote_types::{PollConfig, RevealCommittee};

    pub fn mock_env<U: Into<HumanAddr>>(sender: U, sent: &[Coin], block: u64, time: u64) -> Env {
        Env {
            block: BlockInfo {
                height: block,
                time,
                chain_id: "cosmos-testnet-14002".to_string(),
            },
            message: MessageInfo {
                sender: sender.into(),
                sent_funds: sent.to_vec(),
            },
            contract: ContractInfo {
                address: HumanAddr::from(MOCK_CONTRACT_ADDR),
            },
            contract_key: Some("".to_string()),
            contract_code_hash: "".to_string(),
        }
    }

    fn init_helper() -> (
        StdResult<InitResponse>,
        Extern<MockStorage, MockApi, MockQuerier>,
    ) {
        let mut deps = mock_dependencies(20, &[]);
        let env = mock_env("factory", &[], 0, 0);

        let init_msg = PollInitMsg {
            metadata: PollMetadata {
                title: "test vote".to_string(),
                description: "hey hey this is a test vote".to_string(),
                vote_type: "cool type".to_string(),
                author_addr: Some(HumanAddr("proposer".to_string())),
                author_alias: "proposer".into(),
            },
            config: PollConfig {
                duration: 1000,
                quorum: 33,
                min_threshold: 0,
            },
            reveal_com: RevealCommittee {
                n: 2,
                revealers: vec![HumanAddr("rev1".into()), HumanAddr("rev2".into())],
            },
            choices: vec!["Yes".into(), "No".into()],
            staking_pool: SecretContract {
                address: HumanAddr("staking pool".to_string()),
                contract_hash: "".to_string(),
            },
            init_hook: None,
        };

        (init(&mut deps, env, init_msg), deps)
    }

    #[test]
    fn test_vote_info() {
        let mut deps = mock_dependencies(20, &[]);
        let env = mock_env("factory", &[], 0, 0);
        let init_msg = PollInitMsg {
            metadata: PollMetadata {
                title: "test_vote_info".to_string(),
                description: "test_vote_info".to_string(),
                vote_type: "cool type".to_string(),
                author_addr: Some(HumanAddr("proposer".to_string())),
                author_alias: "proposer".into(),
            },
            config: PollConfig {
                duration: 1000,
                quorum: 33,
                min_threshold: 0,
            },
            reveal_com: RevealCommittee {
                n: 2,
                revealers: vec![HumanAddr("rev1".into()), HumanAddr("rev2".into())],
            },
            choices: vec!["Yes".into(), "No".into()],
            staking_pool: SecretContract {
                address: HumanAddr("staking pool".to_string()),
                contract_hash: "".to_string(),
            },
            init_hook: None,
        };
        init(&mut deps, env, init_msg).unwrap();

        let res = query_vote_info(&deps).unwrap();
        assert_eq!(
            res,
            to_binary(&QueryAnswer::VoteInfo {
                metadata: PollMetadata {
                    title: "test_vote_info".to_string(),
                    description: "test_vote_info".to_string(),
                    vote_type: "cool type".to_string(),
                    author_addr: Some(HumanAddr("proposer".to_string())),
                    author_alias: "proposer".into(),
                },
                config: StoredPollConfig {
                    end_timestamp: 1000,
                    quorum: 33,
                    min_threshold: 0,
                    choices: vec!["Yes".into(), "No".into()],
                    finalized: false,
                    valid: false,
                    rolling_hash: [0u8; 32]
                },
                reveal_com: RevealCommittee {
                    n: 2,
                    revealers: vec![HumanAddr("rev1".into()), HumanAddr("rev2".into())],
                }
            })
            .unwrap()
        )
    }

    #[test]
    fn test_tally() {
        let (init_result, mut deps) = init_helper();
        assert!(init_result.is_ok());

        update_vote(
            &mut deps,
            &HumanAddr("user".into()),
            None,
            Vote {
                choice: 0,
                voting_power: 100,
            },
        )
        .unwrap();

        let err = query_tally(&deps).unwrap_err();
        assert_eq!(err, StdError::generic_err("vote hasn't been finalized yet"));

        // Finalize
        let mut config: StoredPollConfig = TypedStoreMut::attach(&mut deps.storage)
            .load(CONFIG_KEY)
            .unwrap();
        config.valid = true;
        config.finalized = true;
        TypedStoreMut::attach(&mut deps.storage)
            .store(CONFIG_KEY, &config)
            .unwrap();

        let res = query_tally(&deps).unwrap();
        assert_eq!(
            res,
            to_binary(&QueryAnswer::Tally {
                choices: vec!["Yes".into(), "No".into()],
                tally: vec![Uint128(100), Uint128(0)],
            })
            .unwrap()
        )
    }

    #[test]
    fn test_tally_before_ended() {}

    #[test]
    fn test_tally_below_quorum() {}

    #[test]
    fn test_minimum_deposit() {}

    #[test]
    fn test_has_voted() {}

    #[test]
    fn test_num_of_voters() {}

    #[test]
    fn test_query_vote() {}

    #[test]
    fn test_update_voting_power() {}

    #[test]
    fn test_vote_after_ended() {}

    #[test]
    fn test_finalize_before_ended() {}
}
