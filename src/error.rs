use cosmwasm_std::StdError;
use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum ContractError {
    #[error("{0}")]
    Std(#[from] StdError),

    #[error("Unauthorized")]
    Unauthorized {},

    #[error("entries are paused")]
    Paused {},

    #[error("round {round_id} not found")]
    RoundNotFound { round_id: u64 },

    #[error("seed_hash must be exactly 32 bytes, got {got}")]
    BadSeedHash { got: usize },

    #[error("entropy must be 1..={max} bytes")]
    BadEntropy { max: usize },

    #[error("entries must be greater than zero")]
    ZeroEntries {},

    #[error("close_time must be after the current block time")]
    CloseTimeInPast {},

    #[error("close_time must be later than the previous round's ({previous})")]
    CloseTimeNotAfterPrevious { previous: u64 },

    #[error("round {round_id} is already settled")]
    AlreadySettled { round_id: u64 },

    #[error("round {expected} must be settled first — settlement is strictly in order")]
    OutOfOrder { expected: u64 },

    #[error("round {round_id} has not closed yet")]
    NotClosed { round_id: u64 },

    #[error("secret does not match the committed seed_hash")]
    SecretMismatch {},

    #[error("round {round_id} is not stale yet: rollover opens {secs}s after close_time")]
    NotStale { round_id: u64, secs: u64 },

    #[error("invalid config: {reason}")]
    InvalidConfig { reason: String },
}
