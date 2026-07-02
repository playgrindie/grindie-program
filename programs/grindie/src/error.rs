use anchor_lang::prelude::*;

#[error_code]
pub enum GrindieError {
    #[msg("program is paused")]
    Paused,
    #[msg("only the admin may do this")]
    Unauthorized,
    #[msg("bps split must sum to 10000")]
    BpsSumInvalid,
    #[msg("buy-in outside [min,max]")]
    BuyinOutOfRange,
    #[msg("wallet does not hold enough $GRINDIE for ARENA")]
    NotHolder,
    #[msg("session is already finalized")]
    SessionFinalized,
    #[msg("invalid ending id")]
    BadEnding,
    #[msg("arithmetic overflow")]
    Overflow,
}
