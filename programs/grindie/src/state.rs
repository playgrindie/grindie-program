use anchor_lang::prelude::*;

// ---- constants ---------------------------------------------------------

/// ending ids (record-only; classification happens off-chain on the backend)
pub const END_REKT_FOREVER: u8 = 0;
pub const END_GIGA_GRINDER: u8 = 1;
pub const END_MADE_IT: u8 = 2;
pub const END_REKT_BUT_BUILT: u8 = 3;
pub const END_SURVIVED: u8 = 4;
pub const END_REKT: u8 = 5;
pub const END_MAX: u8 = 5;

// (achievement ids + AchievementReceipt live in the achievements-v1 update branch)

/// session status
pub const STATUS_ACTIVE: u8 = 0;
pub const STATUS_FINALIZED: u8 = 1;

// ---- accounts ----------------------------------------------------------

#[account]
pub struct Config {
    pub admin: Pubkey,
    pub mint: Pubkey,
    /// backend signer that attests off-chain session results (bounded oracle)
    pub outcome_authority: Pubkey,
    /// hash/merkle-root of the active season content (provenance only)
    pub content_root: [u8; 32],
    pub holder_threshold: u64,
    pub buyin_min: u64,
    pub buyin_max: u64,
    pub session_fee: u64,
    pub nft_mint_fee: u64,
    pub rekt_forever_burn: u64,
    pub fee_burn_bps: u16,
    pub fee_treasury_bps: u16,
    pub win_share_bps: u16,
    pub win_burn_bps: u16,
    pub loss_vault_bps: u16,
    pub loss_burn_bps: u16,
    /// share of the Vault paid out per weekly jackpot draw (Update 2)
    pub jackpot_share_bps: u16,
    pub win_cap_mult: u8,
    pub paused: bool,
    pub bump: u8,
    pub vault_bump: u8,
    pub treasury_bump: u8,
}
impl Config {
    pub const LEN: usize = 32 * 3 // pubkeys
        + 32          // content_root
        + 8 * 6       // u64
        + 2 * 7       // u16
        + 1 * 5;      // u8/bool (win_cap_mult, paused, bump, vault_bump, treasury_bump)
}

/// ARENA session — money custody + finalized result record. Gameplay is off-chain.
#[account]
pub struct Session {
    pub player: Pubkey,
    pub session_id: u64,
    pub buyin: u64,        // D
    pub start_stack: u64,  // == buyin
    pub final_stack: u64,  // S (set at finalize, from backend)
    pub choices_hash: [u8; 32], // provenance of the off-chain run
    pub status: u8,
    pub ending: u8,        // 0xFF until finalized
    pub created_slot: u64,
    pub bump: u8,
    pub escrow_bump: u8,
}
impl Session {
    pub const LEN: usize = 32 + 8 + 8 + 8 + 8 + 32 + 1 + 1 + 8 + 1 + 1;
}

/// one-per-(player,session) receipt that an ending NFT was minted (dedup).
#[account]
pub struct NftReceipt {
    pub player: Pubkey,
    pub session_id: u64,
    pub ending: u8,
    pub bump: u8,
}
impl NftReceipt {
    pub const LEN: usize = 32 + 8 + 1 + 1;
}


// ---- instruction params ------------------------------------------------

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct ConfigParams {
    pub holder_threshold: u64,
    pub buyin_min: u64,
    pub buyin_max: u64,
    pub session_fee: u64,
    pub nft_mint_fee: u64,
    pub rekt_forever_burn: u64,
    pub fee_burn_bps: u16,
    pub fee_treasury_bps: u16,
    pub win_share_bps: u16,
    pub win_burn_bps: u16,
    pub loss_vault_bps: u16,
    pub loss_burn_bps: u16,
    pub jackpot_share_bps: u16,
    pub win_cap_mult: u8,
}
