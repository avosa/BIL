use std::hash::Hasher;
use rapidhash::RapidHasher;
use windoge_pow_backend::memory::{
    add_balance,
    add_state,
    all_blocks,
    all_stats,
    block_count,
    get_balance,
    get_last_state,
    get_stat,
    insert_block,
    insert_new_miner,
    insert_new_transaction,
    insert_stats,
    latest_block,
    miner_count,
    sub_balance,
    transaction_count,
    Block,
    Stats,
    Transaction,
    TransactionArgs,
};
use windoge_pow_backend::miner::{ create_canister, install_code };
use windoge_pow_backend::{
    miner_wasm,
    mutate_state,
    read_state,
    replace_state,
    State,
    BIL_LEDGER_ID,
    BLOCK_HALVING,
    SEC_NANOS,
};
use candid::{ CandidType, Decode, Encode, Principal };
use ic_cdk::{ init, post_upgrade, pre_upgrade, query, update };

const WINDOGE_LEDGER_ID: &str = "rh2pm-ryaaa-aaaan-qeniq-cai";
const WINDOGE_RECEIVER: &str = "zp2fk-qfdts-3jpq4-oe2lv-xphrr-akxnj-dgtwc-f2psp-wsomh-e5gyz-aae";
const WINDOGE_MINER_CREATION_AMOUNT: u64 = 1500000000; // 15 Windoge98
const BLOCK_TIME: u64 = 300 * SEC_NANOS; // 5 minutes
const MAX_DIFFICULTY: u32 = 48;
const MIN_DIFFICULTY: u32 = 24;
const TRANSACTION_LIMIT: usize = 150;
const BLOCK_BATCH_SIZE: usize = 100;

fn main() {}

#[init]
fn init() {
    let state = State::new();
    replace_state(state);

    let block = Block::genesis();
    let _ = insert_block(block);

    start_next_block(1);
}

#[pre_upgrade]
fn pre_upgrade() {
    let state = read_state(|s| s.clone());
    let _ = add_state(state);
}

#[post_upgrade]
fn post_upgrade() {
    if let Some(old_state) = get_last_state() {
        replace_state(old_state);
    }

    mutate_state(|s| {
        s.bil_ledger_id = Principal::from_text(BIL_LEDGER_ID).unwrap();
    });

    start_next_block(1);
}

#[query]
fn get_latest_block() -> Option<Block> {
    latest_block()
}

#[query]
fn get_all_blocks() -> Vec<Block> {
    all_blocks()
}

#[query]
fn get_all_stats() -> Vec<Stats> {
    all_stats()
}

#[query]
fn get_stats(index: u64) -> Option<Stats> {
    get_stat(index)
}

#[query]
fn get_difficulty() -> u32 {
    read_state(|s| s.current_difficulty)
}

#[query]
fn get_current_rewards() -> u64 {
    read_state(|s| s.current_rewards())
}

#[query]
fn get_next_halving() -> u64 {
    let mined_blocks = read_state(|s| s.mined_block_count());
    let blocks_to_next_halving = BLOCK_HALVING - (mined_blocks % BLOCK_HALVING);
    blocks_to_next_halving
}

#[query]
fn get_mempool() -> Vec<Transaction> {
    read_state(|s| s.mempool.clone())
}

#[query]
pub fn get_state() -> State {
    read_state(|s| s.clone())
}

#[query]
fn get_balance_of(user: Principal) -> u64 {
    get_balance(user)
}

#[query]
fn get_miners(user: Principal) -> Vec<Principal> {
    read_state(|s| s.principal_to_miner.get(&user).cloned().unwrap_or_default())
}

#[query]
fn get_miner_count() -> u64 {
    miner_count()
}

#[query]
fn get_transaction_count() -> u64 {
    transaction_count()
}

#[update]
async fn topup_miner(miner: Principal, block_index: u64) -> Result<String, String> {
    if ic_cdk::caller() == Principal::anonymous() {
        return Err("caller is anonymous".to_string());
    }

    if let Some(_) = read_state(|s| s.miner_to_owner.get(&miner).cloned()) {
        if read_state(|s| s.miner_creation_transactions.contains(&block_index)) {
            return Err("transaction already processed".to_string());
        }

        let index = candid::Nat::from(block_index);
        let transaction = fetch_block(index).await?;

        if let Some(transfer) = transaction.transfer {
            if transfer.from.owner != ic_cdk::caller() {
                return Err("transfer not from caller".to_string());
            }
            if transfer.to.owner != ic_cdk::id() {
                return Err("transfer not to BIL canister".to_string());
            }

            let burn_amount = nat_to_u64((transfer.amount.clone() * 10_u128) / 100_u128)?;
            match
                burn_exe(BurnArgs {
                    memo: None,
                    from_subaccount: None,
                    created_at_time: None,
                    amount: candid::Nat::from(burn_amount),
                }).await
            {
                Ok(index) => {
                    ic_cdk::println!("Burned {} EXE, index: {:?}", burn_amount, index);
                    mutate_state(|s| {
                        s.exe_burned += burn_amount;
                    });
                }
                Err(e) => ic_cdk::println!("Error burning {} EXE: {:?}", burn_amount, e),
            }

            let cycles_amount = tokens_to_cycles(nat_to_u64(transfer.amount)?);

            match transfer_cycles(miner, (cycles_amount * 80) / 100).await {
                Ok(_) => {
                    let _ = insert_new_transaction(block_index);
                    mutate_state(|s| {
                        s.miner_creation_transactions.insert(block_index);
                    });
                    ic_cdk::println!("Topped up miner {}", miner.to_text());
                    return Ok("topped up miner".to_string());
                }
                Err(e) => {
                    ic_cdk::println!("Error topping up miner: {:?}", e);
                    return Err("error topping up miner".to_string());
                }
            }
        } else {
            return Err("expected transfer".to_string());
        }
    } else {
        return Err("miner not found".to_string());
    }
}

#[update]
async fn create_transaction(transaction_arg: TransactionArgs) -> Result<String, String> {
    if ic_cdk::caller() == Principal::anonymous() {
        return Err("caller is anonymous".to_string());
    }

    let pending_amount = read_state(|s|
        s.pending_balance.get(&ic_cdk::caller()).cloned().unwrap_or(0)
    );
    let actual_balance = get_balance(ic_cdk::caller());

    if actual_balance < transaction_arg.amount + pending_amount {
        return Err("insufficient balance".to_string());
    }

    if read_state(|s| s.mempool.len()) > TRANSACTION_LIMIT {
        return Err("network is congested, transactions can be processed in next block".to_string());
    }

    if transaction_arg.amount < 1 && transaction_arg.recipient == ic_cdk::id() {
        return Err("amount must be greater than 0".to_string());
    }

    mutate_state(|s| {
        s.pending_balance
            .entry(ic_cdk::caller())
            .and_modify(|e| {
                *e += transaction_arg.amount;
            })
            .or_insert(transaction_arg.amount);
    });

    let transaction = Transaction {
        sender: ic_cdk::caller(),
        recipient: transaction_arg.recipient,
        amount: transaction_arg.amount,
        timestamp: ic_cdk::api::time(),
    };

    mutate_state(|s| {
        s.mempool.push(transaction.clone());
    });

    Ok("transaction created".to_string())
}

#[update]
async fn spawn_miner(block_index: u64) -> Result<Principal, String> {
    if ic_cdk::caller() == Principal::anonymous() {
        return Err("caller is anonymous".to_string());
    }

    if read_state(|s| s.miner_creation_transactions.contains(&block_index)) {
        return Err("transaction already processed".to_string());
    }

    let index = candid::Nat::from(block_index);
    let transaction = fetch_block(index).await?;

    if let Some(transfer) = transaction.transfer {
        if transfer.from.owner != ic_cdk::caller() {
            return Err("transfer not from caller".to_string());
        }
        if transfer.amount < WINDOGE_MINER_CREATION_AMOUNT {
            return Err("transfer amount too low".to_string());
        }
        if transfer.to.owner != ic_cdk::id() {
            return Err("transfer not to BIL canister".to_string());
        }
    } else {
        return Err("expected transfer".to_string());
    }

    let arg = Encode!(&ic_cdk::caller()).unwrap();

    let canister_id = create_canister(2_500_000_000_000).await.map_err(|e|
        format!("{} - {:?}", e.method, e.reason)
    )?;

    install_code(canister_id, miner_wasm().to_vec(), arg).await.map_err(|e|
        format!("{} - {:?}", e.method, e.reason)
    )?;

    mutate_state(|s| {
        s.new_miner(canister_id, ic_cdk::caller(), block_index);
    });
    insert_new_miner(canister_id, ic_cdk::caller(), block_index);
    let _ = insert_new_transaction(block_index);

    match
        burn_exe(BurnArgs {
            memo: None,
            from_subaccount: None,
            created_at_time: None,
            amount: ((WINDOGE_MINER_CREATION_AMOUNT * 40) / 100).into(),
        }).await
    {
        Ok(index) => {
            ic_cdk::println!(
                "Burned {} EXE, index: {:?}",
                (WINDOGE_MINER_CREATION_AMOUNT * 40) / 100,
                index
            );
            mutate_state(|s| {
                s.exe_burned += (WINDOGE_MINER_CREATION_AMOUNT * 40) / 100;
            });
        }
        Err(e) => ic_cdk::println!("Error burning EXE: {:?}", e),
    }

    if let Some(block) = read_state(|s| s.current_block.clone()) {
        ic_cdk::spawn(async move {
            let _: Result<(), _> = ic_cdk::api::call::call(canister_id, "push_block", (
                block,
                2_u32,
            )).await;
        });
    }

    ic_cdk::println!("Miner {} spawned", canister_id.to_text());

    Ok(canister_id)
}

#[update(hidden = true)]
async fn submit_solution(block: Block, stats: Stats) -> Result<bool, String> {
    if let Err(e) = validate_solution(&block) {
        ic_cdk::println!("Solution from miner {} rejected: {}", ic_cdk::caller().to_text(), e);
        return Err(e);
    }

    mutate_state(|s| {
        s.miner_to_mined_block
            .entry(ic_cdk::caller())
            .and_modify(|e| {
                *e += 1;
            })
            .or_insert(1);

        s.miner_to_burned_cycles
            .entry(ic_cdk::caller())
            .and_modify(|e| {
                *e += stats.cycles_burned;
            })
            .or_insert(stats.cycles_burned);

        s.average_block_time =
            (s.average_block_time * (block.header.height - 1) + stats.solve_time) /
            block.header.height;

        s.block_height = block.header.height;
        s.transaction_count += block.transactions.len() as u64;

        for tx in &block.transactions {
            if let Some(pos) = s.mempool.iter().position(|x| x == tx) {
                s.mempool.remove(pos);
            }

            if let Some(_) = s.pending_balance.get(&tx.sender) {
                s.pending_balance.entry(tx.sender).and_modify(|e| {
                    *e -= tx.amount;
                });
            }
        }
    });

    let _ = insert_block(block.clone());
    let _ = insert_stats(stats.clone());

    for transaction in block.transactions {
        if transaction.recipient == ic_cdk::id() {
            let transfer = TransferArg {
                to: Account {
                    owner: transaction.sender,
                    subaccount: None,
                },
                fee: None,
                from_subaccount: None,
                memo: None,
                created_at_time: None,
                amount: transaction.amount.into(),
            };
            ic_cdk::spawn(async move {
                match icrc1_transfer(transfer, Principal::from_text(BIL_LEDGER_ID).unwrap()).await {
                    Ok(_) => {
                        ic_cdk::println!("BIL minted successfully");
                        sub_balance(transaction.sender, transaction.amount);
                    }
                    Err(e) => {
                        ic_cdk::println!("Error minting BIL: {:?}", e);
                    }
                }
            });
        } else {
            add_balance(transaction.recipient, transaction.amount);
            sub_balance(transaction.sender, transaction.amount);
        }
    }

    let miner_owner = read_state(|s|
        s.miner_to_owner.get(&ic_cdk::caller()).cloned().unwrap_or(Principal::anonymous())
    );
    add_balance(
        miner_owner,
        read_state(|s| s.current_rewards())
    );

    if BLOCK_TIME > stats.solve_time {
        let sec = (BLOCK_TIME - stats.solve_time) / SEC_NANOS;
        if sec > 60 && read_state(|s| s.current_difficulty) < MAX_DIFFICULTY {
            mutate_state(|s| {
                s.current_difficulty = s.current_difficulty + 1;
            });
        }
    } else {
        let sec = (stats.solve_time - BLOCK_TIME) / SEC_NANOS;
        if sec > 60 && read_state(|s| s.current_difficulty) > MIN_DIFFICULTY {
            mutate_state(|s| {
                s.current_difficulty = s.current_difficulty - 1;
            });
        }
    }

    ic_cdk::println!("Solution from miner {} accepted", ic_cdk::caller().to_text());

    start_next_block(1);

    Ok(true)
}

fn validate_solution(block: &Block) -> Result<(), String> {
    if ic_cdk::caller() == Principal::anonymous() {
        return Err("caller is anonymous".to_string());
    }

    if !read_state(|s| s.miner_to_owner.contains_key(&ic_cdk::caller())) {
        return Err("Unregistered miner".to_string());
    }

    if let Some(latest_block) = latest_block() {
        if block.header.prev_hash != latest_block.hash {
            return Err("Block references outdated chain state".to_string());
        }
    } else {
        if block.header.prev_hash != 0 {
            return Err("Block references outdated chain state".to_string());
        }
    }

    if block.header.height != block_count() {
        return Err("Block height mismatch".to_string());
    }

    let mut hasher = RapidHasher::new(0);
    let mut data = Vec::new();
    data.extend_from_slice(&block.header.version.to_le_bytes());
    data.extend_from_slice(&block.header.prev_hash.to_le_bytes());
    data.extend_from_slice(&block.header.merkle_root.to_le_bytes());
    data.extend_from_slice(&block.header.timestamp.to_le_bytes());
    data.extend_from_slice(&block.nonce.to_le_bytes());

    hasher.write(&data);
    let hash64 = hasher.finish();

    let hash128_high = {
        let mut hasher = RapidHasher::new(hash64);
        hasher.write(&hash64.to_le_bytes());
        hasher.finish()
    };

    let hash_value = ((hash128_high as u128) << 64) | (hash64 as u128);
    if hash_value.leading_zeros() < block.header.difficulty {
        return Err("Invalid solution".to_string());
    }

    Ok(())
}

#[update(hidden = true)]
async fn distribute_block(block: Block, start: u64) -> Result<(), String> {
    if ic_cdk::caller() != ic_cdk::id() {
        return Err("caller is not allowed".to_string());
    }

    let miners = read_state(|s| s.miner_to_owner.keys().cloned().collect::<Vec<Principal>>());
    let batch_start = std::cmp::min(start as usize, miners.len());
    let batch_end = std::cmp::min((batch_start as usize) + BLOCK_BATCH_SIZE, miners.len());

    ic_cdk::println!("Distributing block to miners {} to {}", batch_start, batch_end);
    
    for i in batch_start..batch_end {
        let miner = &miners[i];
        push_block(block.clone(), miner.clone(), (i + 1) as u32);
    }

    if batch_end < miners.len() {
        ic_cdk::spawn(async move {
            let _: Result<(), _> = ic_cdk::api::call::call(ic_cdk::id(), "distribute_block", (
                block,
                batch_end as u64,
            )).await;
        });
    }

    Ok(())
}

fn push_block(block: Block, miner: Principal, miner_id: u32) {
    ic_cdk::spawn(async move {
        let _: Result<(), _> = ic_cdk::api::call::call(miner, "push_block", (
            block,
            miner_id,
        )).await;
    });
}

fn start_next_block(sec: u64) {
    ic_cdk::println!("Starting next block in {} seconds", sec);
    ic_cdk_timers::set_timer(std::time::Duration::from_secs(sec), || {
        create_block();
    });
}

fn create_block() {
    let transactions = read_state(|s| s.mempool.clone());
    if transactions.is_empty() {
        ic_cdk::println!("No transactions to include in block");
        start_next_block(20);
        return;
    }

    ic_cdk::println!("Creating block with {} transactions", transactions.len());

    let prev_block = latest_block().unwrap();
    let difficulty = read_state(|s| s.current_difficulty);
    match Block::new(&prev_block, transactions, difficulty) {
        Ok(block) => {
            ic_cdk::println!("Block created successfully!");
            mutate_state(|s| {
                s.current_block = Some(block.clone());
            });
            ic_cdk::spawn(async move {
                let _: Result<(), _> = ic_cdk::api::call::call(ic_cdk::id(), "distribute_block", (
                    block,
                    0 as u64
                )).await;
            });
        }
        Err(e) => {
            ic_cdk::println!("Error creating block: {:?}", e);
        }
    }
}

#[derive(CandidType, candid::Deserialize)]
pub struct Account {
    pub owner: Principal,
    pub subaccount: Option<serde_bytes::ByteBuf>,
}

#[derive(CandidType, candid::Deserialize)]
pub struct Burn {
    pub from: Account,
    pub memo: Option<serde_bytes::ByteBuf>,
    pub created_at_time: Option<u64>,
    pub amount: candid::Nat,
}

#[derive(CandidType, candid::Deserialize)]
pub struct Mint1 {
    pub to: Account,
    pub memo: Option<serde_bytes::ByteBuf>,
    pub created_at_time: Option<u64>,
    pub amount: candid::Nat,
}

#[derive(CandidType, candid::Deserialize)]
pub struct Transfer {
    pub to: Account,
    pub fee: Option<candid::Nat>,
    pub from: Account,
    pub memo: Option<serde_bytes::ByteBuf>,
    pub created_at_time: Option<u64>,
    pub amount: candid::Nat,
}

#[derive(CandidType, candid::Deserialize)]
pub struct Transaction1 {
    pub burn: Option<Burn>,
    pub kind: String,
    pub mint: Option<Mint1>,
    pub timestamp: u64,
    pub index: candid::Nat,
    pub transfer: Option<Transfer>,
}

async fn fetch_block(block_height: candid::Nat) -> Result<Transaction1, String> {
    let result: Result<Vec<u8>, (i32, String)> = ic_cdk::api::call
        ::call_raw(
            Principal::from_text(WINDOGE_LEDGER_ID).unwrap(),
            "get_transaction",
            candid::encode_args((block_height,)).unwrap(),
            0
        ).await
        .map_err(|(code, msg)| (code as i32, msg));
    match result {
        Ok(res) => {
            let block = Decode!(&res, Option<Transaction1>).map_err(|e| format!("{:?}", e))?;
            if let Some(block) = block {
                ic_cdk::println!("transaction: {:?}", block.index);
                Ok(block)
            } else {
                Err("Block not found".to_string())
            }
        }
        Err((code, msg)) =>
            Err(format!("Error while calling minter canister ({}): {:?}", code, msg)),
    }
}

#[derive(CandidType, candid::Deserialize)]
struct TransferArg {
    pub to: Account,
    pub fee: Option<candid::Nat>,
    pub memo: Option<serde_bytes::ByteBuf>,
    pub from_subaccount: Option<serde_bytes::ByteBuf>,
    pub created_at_time: Option<u64>,
    pub amount: candid::Nat,
}

#[derive(CandidType, candid::Deserialize, Debug)]
enum TransferError {
    GenericError {
        message: String,
        error_code: candid::Nat,
    },
    TemporarilyUnavailable,
    BadBurn {
        min_burn_amount: candid::Nat,
    },
    Duplicate {
        duplicate_of: candid::Nat,
    },
    BadFee {
        expected_fee: candid::Nat,
    },
    CreatedInFuture {
        ledger_time: u64,
    },
    TooOld,
    InsufficientFunds {
        balance: candid::Nat,
    },
}

#[derive(CandidType, candid::Deserialize)]
enum Result_ {
    Ok(candid::Nat),
    Err(TransferError),
}

async fn icrc1_transfer(transfer: TransferArg, token: Principal) -> Result<candid::Nat, String> {
    let result: Result<Vec<u8>, (i32, String)> = ic_cdk::api::call
        ::call_raw(token, "icrc1_transfer", candid::encode_args((transfer,)).unwrap(), 0).await
        .map_err(|(code, msg)| (code as i32, msg));
    match result {
        Ok(res) => {
            let response = Decode!(&res, Result_).map_err(|e| format!("{:?}", e))?;
            match response {
                Result_::Ok(index) => Ok(index),
                Result_::Err(e) => Err(format!("{:?}", e)),
            }
        }
        Err((code, msg)) => Err(format!("Error icrc1_transfer ({}): {:?}", code, msg)),
    }
}

#[update(hidden = true)]
async fn transfer_exe(amount: u64) -> Result<candid::Nat, String> {
    if ic_cdk::caller() != Principal::from_text(WINDOGE_RECEIVER).unwrap() {
        return Err("caller is not allowed".to_string());
    }

    let transfer = TransferArg {
        to: Account {
            owner: Principal::from_text(WINDOGE_RECEIVER).unwrap(),
            subaccount: None,
        },
        fee: None,
        memo: None,
        from_subaccount: None,
        created_at_time: None,
        amount: candid::Nat::from(amount),
    };

    match icrc1_transfer(transfer, Principal::from_text(WINDOGE_LEDGER_ID).unwrap()).await {
        Ok(index) => Ok(index),
        Err(e) => Err(e),
    }
}

type Balance = candid::Nat;
type Subaccount = serde_bytes::ByteBuf;
#[derive(CandidType, candid::Deserialize)]
struct BurnArgs {
    memo: Option<serde_bytes::ByteBuf>,
    from_subaccount: Option<Subaccount>,
    created_at_time: Option<u64>,
    amount: Balance,
}

type TxIndex = candid::Nat;
type Timestamp = u64;
#[derive(CandidType, candid::Deserialize, Debug)]
pub enum TransferError1 {
    GenericError {
        message: String,
        error_code: candid::Nat,
    },
    TemporarilyUnavailable,
    BadBurn {
        min_burn_amount: Balance,
    },
    Duplicate {
        duplicate_of: TxIndex,
    },
    BadFee {
        expected_fee: Balance,
    },
    CreatedInFuture {
        ledger_time: Timestamp,
    },
    TooOld,
    InsufficientFunds {
        balance: Balance,
    },
}

#[derive(CandidType, candid::Deserialize)]
pub enum TransferResult {
    Ok(TxIndex),
    Err(TransferError1),
}

async fn burn_exe(args: BurnArgs) -> Result<candid::Nat, String> {
    let result: Result<Vec<u8>, (i32, String)> = ic_cdk::api::call
        ::call_raw(
            Principal::from_text(WINDOGE_LEDGER_ID).unwrap(),
            "burn",
            candid::encode_args((args,)).unwrap(),
            0
        ).await
        .map_err(|(code, msg)| (code as i32, msg));
    match result {
        Ok(res) => {
            let result = Decode!(&res, TransferResult).map_err(|e| format!("{:?}", e))?;
            match result {
                TransferResult::Ok(index) => {
                    return Ok(index);
                }
                TransferResult::Err(e) => Err(format!("burn: {:?}", e)),
            }
        }
        Err((code, msg)) =>
            Err(format!("Error while calling minter canister ({}): {:?}", code, msg)),
    }
}

async fn transfer_cycles(canister: Principal, amount: u64) -> Result<(), String> {
    match ic_cdk::api::call::call_with_payment::<(), ()>(canister, "receive", (), amount).await {
        Ok(_) => Ok(()),
        Err((code, msg)) => {
            Err(format!("transfer_cycles failed with code: {:?}, message: {}", code, msg))
        }
    }
}

fn tokens_to_cycles(token_amount: u64) -> u64 {
    let actual_tokens = (token_amount as f64) / 100000000.0;
    let dollars = actual_tokens * 0.6;
    let cycles_per_dollar = 1_000_000_000_000_f64 / 1.35;
    let total_cycles = dollars * cycles_per_dollar;
    total_cycles as u64
}

#[derive(CandidType, Ord, PartialOrd, Eq, PartialEq, Clone)]
struct LeaderBoardEntry {
    owner: Principal,
    miner_count: usize,
    block_count: u64,
}

#[query]
fn get_leaderboard() -> Vec<LeaderBoardEntry> {
    use std::collections::BTreeSet;

    read_state(|s| {
        let mut principal_counts: Vec<(Principal, u64)> = s.miner_to_mined_block
            .iter()
            .map(|(principal, block_count)| (principal.clone(), block_count.clone()))
            .collect();
        principal_counts.sort_by(|a, b| b.1.cmp(&a.1));
        let biggest: Vec<(Principal, u64)> = principal_counts.into_iter().take(20).collect();

        let mut result: BTreeSet<LeaderBoardEntry> = BTreeSet::default();

        for (miner, _block_count) in biggest {
            let (owner, block_count, miner_count): (Principal, u64, usize) = match
                s.miner_to_owner.get(&miner)
            {
                Some(owner) => {
                    let block_count = s.principal_to_miner
                        .get(&owner)
                        .map(|miners| {
                            miners
                                .iter()
                                .map(|miner| *s.miner_to_mined_block.get(miner).unwrap_or(&0))
                                .sum::<u64>()
                        })
                        .unwrap_or(0);

                    let miner_count = s.principal_to_miner
                        .get(&owner)
                        .map(|miners| miners.len())
                        .unwrap_or(0);

                    (owner.clone(), block_count, miner_count)
                }
                None => { (Principal::anonymous(), 0, 0) }
            };
            result.insert(LeaderBoardEntry {
                owner,
                miner_count,
                block_count,
            });
        }

        result.iter().cloned().collect()
    })
}

fn nat_to_u64(nat: candid::Nat) -> Result<u64, String> {
    use num_traits::cast::ToPrimitive;
    nat.0.to_u64().ok_or_else(|| "Failed to convert Nat to u64".to_string())
}
