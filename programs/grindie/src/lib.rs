use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token_interface::{self, Burn, CloseAccount, Mint, TokenAccount, TokenInterface, TransferChecked};
use mpl_core::{instructions::CreateV2CpiBuilder, ID as MPL_CORE_ID};

#[cfg(not(feature = "no-entrypoint"))]
use solana_security_txt::security_txt;

pub mod error;
pub mod state;

use error::GrindieError;
use state::*;

declare_id!("6TchzGfYCmvX7useVcAdt4ewrXdESpPEk2Euyx8Zroom");

#[cfg(not(feature = "no-entrypoint"))]
security_txt! {
    name: "GRINDIE",
    project_url: "https://x.com/GrindieGame",
    contacts: "link:https://x.com/GrindieGame",
    policy: "Report vulnerabilities privately via the contact above and allow reasonable time to patch before public disclosure. Good-faith research is welcome; no formal bug bounty yet.",
    source_code: "https://github.com/playgrindie/grindie-program",
    auditors: "None"
}

// Thin RELEASE contract (base / `main`): ARENA money core + soulbound ending NFT +
// admin teardown. Season content, gameplay, rolls and ending classification live
// off-chain on the backend; the backend co-signs results as `outcome_authority`,
// and the program enforces the money guardrails (buy-in bounds, fee/settlement
// splits, win cap, burns, custody).
//
// The on-chain feature set grows per update branch — same as front/back:
//   • achievements-v1 → `create_achievement_collection`, `mint_achievement`
//   • jackpot-v2      → `draw_jackpot`
//   • referral-v3     → (off-chain, no contract change)
// Each is shipped as an `anchor upgrade` of this same program.
//
// Config layout is FROZEN across every version (upgrades keep the same Config
// account) — `jackpot_share_bps` etc. live here from v0 even though the
// instruction that uses them lands in a later branch.
#[program]
pub mod grindie {
    use super::*;

    // ===== ADMIN =====================================================

    pub fn initialize(ctx: Context<Initialize>, p: ConfigParams) -> Result<()> {
        validate_params(&p)?;
        let c = &mut ctx.accounts.config;
        c.admin = ctx.accounts.admin.key();
        c.mint = ctx.accounts.mint.key();
        c.outcome_authority = ctx.accounts.outcome_authority.key();
        c.content_root = [0u8; 32];
        apply_params(c, &p);
        c.paused = false;
        c.bump = ctx.bumps.config;
        c.vault_bump = ctx.bumps.vault;
        c.treasury_bump = ctx.bumps.treasury;
        Ok(())
    }

    pub fn update_config(ctx: Context<AdminOnly>, p: ConfigParams) -> Result<()> {
        validate_params(&p)?;
        apply_params(&mut ctx.accounts.config, &p);
        Ok(())
    }

    pub fn set_outcome_authority(ctx: Context<AdminOnly>, new_authority: Pubkey) -> Result<()> {
        ctx.accounts.config.outcome_authority = new_authority;
        Ok(())
    }

    /// Transfer the Config admin role (every Config field is admin-updatable).
    pub fn set_admin(ctx: Context<AdminOnly>, new_admin: Pubkey) -> Result<()> {
        ctx.accounts.config.admin = new_admin;
        Ok(())
    }

    /// Change the token mint (e.g. swap a placeholder for the real launch mint).
    /// Do this BEFORE any tokens sit in Vault/Treasury — their ATAs are
    /// mint-specific and old-mint balances would be stranded.
    pub fn set_mint(ctx: Context<AdminOnly>, new_mint: Pubkey) -> Result<()> {
        ctx.accounts.config.mint = new_mint;
        Ok(())
    }

    pub fn set_content_root(ctx: Context<AdminOnly>, root: [u8; 32]) -> Result<()> {
        ctx.accounts.config.content_root = root;
        Ok(())
    }

    pub fn set_paused(ctx: Context<AdminOnly>, paused: bool) -> Result<()> {
        ctx.accounts.config.paused = paused;
        Ok(())
    }

    /// admin tops up the Vault prize pool (from bought/owned tokens).
    pub fn seed_vault(ctx: Context<SeedVault>, amount: u64) -> Result<()> {
        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.source_ata.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.vault_ata.to_account_info(),
                    authority: ctx.accounts.admin.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.mint.decimals,
        )
    }

    pub fn withdraw_treasury(ctx: Context<WithdrawTreasury>, amount: u64) -> Result<()> {
        let bump_a = [ctx.accounts.config.treasury_bump];
        let seeds: &[&[u8]] = &[b"treasury", &bump_a];
        let signer = &[seeds];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.treasury_ata.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.dest_ata.to_account_info(),
                    authority: ctx.accounts.treasury.to_account_info(),
                },
                signer,
            ),
            amount,
            ctx.accounts.mint.decimals,
        )
    }

    /// Admin-only emergency withdrawal from the Vault (prize pool). Same shape as
    /// withdraw_treasury — a trust/centralization point, kept for ops/recovery.
    pub fn withdraw_vault(ctx: Context<WithdrawVault>, amount: u64) -> Result<()> {
        let bump_a = [ctx.accounts.config.vault_bump];
        let seeds: &[&[u8]] = &[b"vault", &bump_a];
        let signer = &[seeds];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.vault_ata.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.dest_ata.to_account_info(),
                    authority: ctx.accounts.vault.to_account_info(),
                },
                signer,
            ),
            amount,
            ctx.accounts.mint.decimals,
        )
    }

    // ===== teardown (rent recovery) ==================================
    // Full clean shutdown returns 100% of what the system holds. Run in order:
    //   1) withdraw_vault + withdraw_treasury  (drain tokens to admin)
    //   2) close_vault_ata + close_treasury_ata (empty ATAs → rent to admin)
    //   3) close_config                         (Config PDA → rent to admin, LAST)
    // Then `solana program close` returns the program rent. After close_config the
    // vault/treasury PDAs can no longer be re-derived usefully — do it last.

    /// Admin closes the (empty) Treasury ATA; rent → admin. Token-2022 CPI.
    pub fn close_treasury_ata(ctx: Context<CloseTreasuryAta>) -> Result<()> {
        let bump_a = [ctx.accounts.config.treasury_bump];
        let seeds: &[&[u8]] = &[b"treasury", &bump_a];
        token_interface::close_account(CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            CloseAccount {
                account: ctx.accounts.treasury_ata.to_account_info(),
                destination: ctx.accounts.admin.to_account_info(),
                authority: ctx.accounts.treasury.to_account_info(),
            },
            &[seeds],
        ))
    }

    /// Admin closes the (empty) Vault ATA; rent → admin. Token-2022 CPI.
    pub fn close_vault_ata(ctx: Context<CloseVaultAta>) -> Result<()> {
        let bump_a = [ctx.accounts.config.vault_bump];
        let seeds: &[&[u8]] = &[b"vault", &bump_a];
        token_interface::close_account(CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            CloseAccount {
                account: ctx.accounts.vault_ata.to_account_info(),
                destination: ctx.accounts.admin.to_account_info(),
                authority: ctx.accounts.vault.to_account_info(),
            },
            &[seeds],
        ))
    }

    /// Admin closes the Config PDA; rent → admin. The teardown terminator — call
    /// after the vault/treasury ATAs are closed (Anchor `close = admin`).
    pub fn close_config(_ctx: Context<CloseConfig>) -> Result<()> {
        Ok(())
    }

    // ===== ARENA money ===============================================

    /// holder pays fee + buy-in; buy-in is custodied in Escrow for the session.
    pub fn start_session_arena(
        ctx: Context<StartArena>,
        _session_id: u64,
        buyin: u64,
    ) -> Result<()> {
        let cfg = &ctx.accounts.config;
        require!(!cfg.paused, GrindieError::Paused);
        require!(buyin >= cfg.buyin_min && buyin <= cfg.buyin_max, GrindieError::BuyinOutOfRange);
        require!(ctx.accounts.player_ata.amount >= cfg.holder_threshold, GrindieError::NotHolder);

        let fee = cfg.session_fee;
        let burn_fee = mul_bps(fee, cfg.fee_burn_bps)?;
        let treasury_fee = fee.checked_sub(burn_fee).ok_or(GrindieError::Overflow)?;
        if burn_fee > 0 {
            burn_from_player(&ctx.accounts.token_program, &ctx.accounts.mint, &ctx.accounts.player_ata, &ctx.accounts.player, burn_fee)?;
        }
        if treasury_fee > 0 {
            xfer_from_player(&ctx.accounts.token_program, &ctx.accounts.mint, &ctx.accounts.player_ata, &ctx.accounts.treasury_ata, &ctx.accounts.player, treasury_fee)?;
        }
        xfer_from_player(&ctx.accounts.token_program, &ctx.accounts.mint, &ctx.accounts.player_ata, &ctx.accounts.escrow_ata, &ctx.accounts.player, buyin)?;

        let s = &mut ctx.accounts.session;
        s.player = ctx.accounts.player.key();
        s.session_id = _session_id;
        s.buyin = buyin;
        s.start_stack = buyin;
        s.final_stack = 0;
        s.choices_hash = [0u8; 32];
        s.status = STATUS_ACTIVE;
        s.ending = 0xFF;
        s.created_slot = Clock::get()?.slot;
        s.bump = ctx.bumps.session;
        s.escrow_bump = ctx.bumps.escrow;
        Ok(())
    }

    /// backend (outcome_authority) co-signs the off-chain result; program settles
    /// money under fixed guardrails (cap 3×, splits, burns). final_stack from backend.
    pub fn finalize_arena(
        ctx: Context<FinalizeArena>,
        final_stack: u64,
        ending: u8,
        choices_hash: [u8; 32],
    ) -> Result<()> {
        require!(ending <= END_MAX, GrindieError::BadEnding);
        let (win_share, loss_burn, cap_mult, rekt_burn, vault_bump) = {
            let c = &ctx.accounts.config;
            (c.win_share_bps, c.loss_burn_bps, c.win_cap_mult, c.rekt_forever_burn, c.vault_bump)
        };
        let session = &mut ctx.accounts.session;
        require!(session.status == STATUS_ACTIVE, GrindieError::SessionFinalized);

        let d = session.buyin;
        let player_key = session.player;
        let sid = session.session_id.to_le_bytes();
        let escrow_bump_a = [session.escrow_bump];
        let vault_bump_a = [vault_bump];
        let escrow_seeds: &[&[u8]] = &[b"escrow", player_key.as_ref(), &sid, &escrow_bump_a];
        let vault_seeds: &[&[u8]] = &[b"vault", &vault_bump_a];

        let tp = ctx.accounts.token_program.to_account_info();
        let mint_ai = ctx.accounts.mint.to_account_info();
        let decimals = ctx.accounts.mint.decimals;
        let escrow_ata = ctx.accounts.escrow_ata.to_account_info();
        let vault_ata = ctx.accounts.vault_ata.to_account_info();
        let player_ata = ctx.accounts.player_ata.to_account_info();
        let escrow_auth = ctx.accounts.escrow.to_account_info();
        let vault_auth = ctx.accounts.vault.to_account_info();

        if final_stack > d {
            let w = final_stack - d;
            let cap = d.checked_mul(cap_mult as u64).ok_or(GrindieError::Overflow)?;
            let wc = w.min(cap);
            let to_player = mul_bps(wc, win_share)?;
            let burn_win = wc.checked_sub(to_player).ok_or(GrindieError::Overflow)?;
            token_interface::transfer_checked(CpiContext::new_with_signer(tp.clone(), TransferChecked { from: escrow_ata.clone(), mint: mint_ai.clone(), to: player_ata.clone(), authority: escrow_auth.clone() }, &[escrow_seeds]), d, decimals)?;
            if to_player > 0 {
                token_interface::transfer_checked(CpiContext::new_with_signer(tp.clone(), TransferChecked { from: vault_ata.clone(), mint: mint_ai.clone(), to: player_ata.clone(), authority: vault_auth.clone() }, &[vault_seeds]), to_player, decimals)?;
            }
            if burn_win > 0 {
                token_interface::burn(CpiContext::new_with_signer(tp.clone(), Burn { mint: mint_ai.clone(), from: vault_ata.clone(), authority: vault_auth.clone() }, &[vault_seeds]), burn_win)?;
            }
        } else {
            let l = d - final_stack;
            let burn_loss = mul_bps(l, loss_burn)?;
            let to_vault = l.checked_sub(burn_loss).ok_or(GrindieError::Overflow)?;
            if final_stack > 0 {
                token_interface::transfer_checked(CpiContext::new_with_signer(tp.clone(), TransferChecked { from: escrow_ata.clone(), mint: mint_ai.clone(), to: player_ata.clone(), authority: escrow_auth.clone() }, &[escrow_seeds]), final_stack, decimals)?;
            }
            if to_vault > 0 {
                token_interface::transfer_checked(CpiContext::new_with_signer(tp.clone(), TransferChecked { from: escrow_ata.clone(), mint: mint_ai.clone(), to: vault_ata.clone(), authority: escrow_auth.clone() }, &[escrow_seeds]), to_vault, decimals)?;
            }
            if burn_loss > 0 {
                token_interface::burn(CpiContext::new_with_signer(tp.clone(), Burn { mint: mint_ai.clone(), from: escrow_ata.clone(), authority: escrow_auth.clone() }, &[escrow_seeds]), burn_loss)?;
            }
        }

        if ending == END_REKT_FOREVER && rekt_burn > 0 {
            let amt = rekt_burn.min(ctx.accounts.vault_ata.amount);
            if amt > 0 {
                token_interface::burn(CpiContext::new_with_signer(tp.clone(), Burn { mint: mint_ai.clone(), from: vault_ata.clone(), authority: vault_auth.clone() }, &[vault_seeds]), amt)?;
            }
        }

        session.final_stack = final_stack;
        session.ending = ending;
        session.choices_hash = choices_hash;
        session.status = STATUS_FINALIZED;
        emit!(SessionFinalized { player: player_key, session_id: session.session_id, ending, final_stack });
        Ok(())
    }

    // ===== ending NFT (soulbound) ====================================

    /// burns the mint fee + mints the soulbound ending asset (Metaplex Core,
    /// frozen via PermanentFreezeDelegate → non-transferable). Dedup via receipt.
    /// `outcome_authority` co-signs to attest the ending.
    pub fn mint_ending_nft(
        ctx: Context<MintEndingNft>,
        session_id: u64,
        ending: u8,
        name: String,
        uri: String,
    ) -> Result<()> {
        require!(ending <= END_MAX, GrindieError::BadEnding);
        require!(name.len() <= 64 && uri.len() <= 200, GrindieError::BadEnding);
        let fee = ctx.accounts.config.nft_mint_fee;
        if fee > 0 {
            burn_from_player(&ctx.accounts.token_program, &ctx.accounts.mint, &ctx.accounts.player_ata, &ctx.accounts.player, fee)?;
        }

        // update_authority = Config PDA, owner = player, no freeze → TRADEABLE
        // (the run's collectible — listable/sellable on Magic Eden etc.).
        CreateV2CpiBuilder::new(&ctx.accounts.mpl_core.to_account_info())
            .asset(&ctx.accounts.asset.to_account_info())
            .payer(&ctx.accounts.player.to_account_info())
            .owner(Some(&ctx.accounts.player.to_account_info()))
            .update_authority(Some(&ctx.accounts.config.to_account_info()))
            .system_program(&ctx.accounts.system_program.to_account_info())
            .name(name)
            .uri(uri)
            .invoke()?;

        let r = &mut ctx.accounts.receipt;
        r.player = ctx.accounts.player.key();
        r.session_id = session_id;
        r.ending = ending;
        r.bump = ctx.bumps.receipt;
        emit!(NftMinted { player: r.player, session_id, ending });
        Ok(())
    }
}

// ===== helpers ======================================================

fn validate_params(p: &ConfigParams) -> Result<()> {
    require!(p.fee_burn_bps + p.fee_treasury_bps == 10_000, GrindieError::BpsSumInvalid);
    require!(p.win_share_bps + p.win_burn_bps == 10_000, GrindieError::BpsSumInvalid);
    require!(p.loss_vault_bps + p.loss_burn_bps == 10_000, GrindieError::BpsSumInvalid);
    require!(p.jackpot_share_bps <= 10_000, GrindieError::BpsSumInvalid);
    require!(p.buyin_min <= p.buyin_max, GrindieError::BuyinOutOfRange);
    Ok(())
}

fn apply_params(c: &mut Config, p: &ConfigParams) {
    c.holder_threshold = p.holder_threshold;
    c.buyin_min = p.buyin_min;
    c.buyin_max = p.buyin_max;
    c.session_fee = p.session_fee;
    c.nft_mint_fee = p.nft_mint_fee;
    c.rekt_forever_burn = p.rekt_forever_burn;
    c.fee_burn_bps = p.fee_burn_bps;
    c.fee_treasury_bps = p.fee_treasury_bps;
    c.win_share_bps = p.win_share_bps;
    c.win_burn_bps = p.win_burn_bps;
    c.loss_vault_bps = p.loss_vault_bps;
    c.loss_burn_bps = p.loss_burn_bps;
    c.jackpot_share_bps = p.jackpot_share_bps;
    c.win_cap_mult = p.win_cap_mult;
}

fn mul_bps(amount: u64, bps: u16) -> Result<u64> {
    Ok(((amount as u128) * (bps as u128) / 10_000) as u64)
}

fn xfer_from_player<'info>(
    tp: &Interface<'info, TokenInterface>,
    mint: &InterfaceAccount<'info, Mint>,
    from: &InterfaceAccount<'info, TokenAccount>,
    to: &InterfaceAccount<'info, TokenAccount>,
    player: &Signer<'info>,
    amount: u64,
) -> Result<()> {
    token_interface::transfer_checked(
        CpiContext::new(tp.to_account_info(), TransferChecked { from: from.to_account_info(), mint: mint.to_account_info(), to: to.to_account_info(), authority: player.to_account_info() }),
        amount,
        mint.decimals,
    )
}

fn burn_from_player<'info>(
    tp: &Interface<'info, TokenInterface>,
    mint: &InterfaceAccount<'info, Mint>,
    from: &InterfaceAccount<'info, TokenAccount>,
    player: &Signer<'info>,
    amount: u64,
) -> Result<()> {
    token_interface::burn(
        CpiContext::new(tp.to_account_info(), Burn { mint: mint.to_account_info(), from: from.to_account_info(), authority: player.to_account_info() }),
        amount,
    )
}

// ===== events =======================================================

#[event]
pub struct SessionFinalized {
    pub player: Pubkey,
    pub session_id: u64,
    pub ending: u8,
    pub final_stack: u64,
}
#[event]
pub struct NftMinted {
    pub player: Pubkey,
    pub session_id: u64,
    pub ending: u8,
}

// ===== account contexts =============================================

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(init, payer = admin, space = 8 + Config::LEN, seeds = [b"config"], bump)]
    pub config: Account<'info, Config>,
    /// CHECK: vault PDA authority
    #[account(seeds = [b"vault"], bump)]
    pub vault: UncheckedAccount<'info>,
    #[account(init, payer = admin, associated_token::mint = mint, associated_token::authority = vault, associated_token::token_program = token_program)]
    pub vault_ata: InterfaceAccount<'info, TokenAccount>,
    /// CHECK: treasury PDA authority
    #[account(seeds = [b"treasury"], bump)]
    pub treasury: UncheckedAccount<'info>,
    #[account(init, payer = admin, associated_token::mint = mint, associated_token::authority = treasury, associated_token::token_program = token_program)]
    pub treasury_ata: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    /// CHECK: stored as the backend signer; not required to sign at init
    pub outcome_authority: UncheckedAccount<'info>,
    #[account(mut)]
    pub admin: Signer<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct AdminOnly<'info> {
    #[account(mut, has_one = admin, seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    pub admin: Signer<'info>,
}

#[derive(Accounts)]
pub struct SeedVault<'info> {
    #[account(has_one = admin, seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, token::mint = config.mint)]
    pub source_ata: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, token::mint = config.mint, token::authority = vault)]
    pub vault_ata: InterfaceAccount<'info, TokenAccount>,
    /// CHECK: vault PDA authority
    #[account(seeds = [b"vault"], bump = config.vault_bump)]
    pub vault: UncheckedAccount<'info>,
    pub admin: Signer<'info>,
    #[account(address = config.mint)]
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct WithdrawTreasury<'info> {
    #[account(has_one = admin, seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, token::mint = config.mint, token::authority = treasury)]
    pub treasury_ata: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, token::mint = config.mint)]
    pub dest_ata: InterfaceAccount<'info, TokenAccount>,
    /// CHECK: treasury PDA authority
    #[account(seeds = [b"treasury"], bump = config.treasury_bump)]
    pub treasury: UncheckedAccount<'info>,
    pub admin: Signer<'info>,
    #[account(address = config.mint)]
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct WithdrawVault<'info> {
    #[account(has_one = admin, seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, token::mint = config.mint, token::authority = vault)]
    pub vault_ata: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, token::mint = config.mint)]
    pub dest_ata: InterfaceAccount<'info, TokenAccount>,
    /// CHECK: vault PDA authority
    #[account(seeds = [b"vault"], bump = config.vault_bump)]
    pub vault: UncheckedAccount<'info>,
    pub admin: Signer<'info>,
    #[account(address = config.mint)]
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct CloseTreasuryAta<'info> {
    #[account(has_one = admin, seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, token::mint = config.mint, token::authority = treasury)]
    pub treasury_ata: InterfaceAccount<'info, TokenAccount>,
    /// CHECK: treasury PDA authority
    #[account(seeds = [b"treasury"], bump = config.treasury_bump)]
    pub treasury: UncheckedAccount<'info>,
    #[account(mut)]
    pub admin: Signer<'info>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct CloseVaultAta<'info> {
    #[account(has_one = admin, seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, token::mint = config.mint, token::authority = vault)]
    pub vault_ata: InterfaceAccount<'info, TokenAccount>,
    /// CHECK: vault PDA authority
    #[account(seeds = [b"vault"], bump = config.vault_bump)]
    pub vault: UncheckedAccount<'info>,
    #[account(mut)]
    pub admin: Signer<'info>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct CloseConfig<'info> {
    #[account(mut, has_one = admin, seeds = [b"config"], bump = config.bump, close = admin)]
    pub config: Account<'info, Config>,
    #[account(mut)]
    pub admin: Signer<'info>,
}

#[derive(Accounts)]
#[instruction(session_id: u64)]
pub struct StartArena<'info> {
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(init, payer = player, space = 8 + Session::LEN, seeds = [b"session", player.key().as_ref(), &session_id.to_le_bytes()], bump)]
    pub session: Account<'info, Session>,
    #[account(mut, token::mint = config.mint, token::authority = player)]
    pub player_ata: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, token::mint = config.mint, token::authority = treasury)]
    pub treasury_ata: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: treasury PDA authority
    #[account(seeds = [b"treasury"], bump = config.treasury_bump)]
    pub treasury: UncheckedAccount<'info>,
    /// CHECK: escrow PDA authority
    #[account(seeds = [b"escrow", player.key().as_ref(), &session_id.to_le_bytes()], bump)]
    pub escrow: UncheckedAccount<'info>,
    #[account(init, payer = player, associated_token::mint = mint, associated_token::authority = escrow, associated_token::token_program = token_program)]
    pub escrow_ata: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, address = config.mint)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut)]
    pub player: Signer<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct FinalizeArena<'info> {
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, has_one = player, seeds = [b"session", player.key().as_ref(), &session.session_id.to_le_bytes()], bump = session.bump)]
    pub session: Box<Account<'info, Session>>,
    #[account(mut, token::mint = config.mint, token::authority = player)]
    pub player_ata: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: escrow PDA authority
    #[account(seeds = [b"escrow", player.key().as_ref(), &session.session_id.to_le_bytes()], bump = session.escrow_bump)]
    pub escrow: UncheckedAccount<'info>,
    #[account(mut, token::mint = config.mint, token::authority = escrow)]
    pub escrow_ata: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: vault PDA authority
    #[account(seeds = [b"vault"], bump = config.vault_bump)]
    pub vault: UncheckedAccount<'info>,
    #[account(mut, token::mint = config.mint, token::authority = vault)]
    pub vault_ata: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, address = config.mint)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: NOT a signer — settlement is operator-driven so a losing player can't
    /// strand their escrow by refusing to sign. The player authorizes nothing here
    /// (all transfers/burns use escrow/vault PDA authorities); validated via the
    /// session `has_one = player` and `player_ata` token::authority = player.
    pub player: UncheckedAccount<'info>,
    /// the backend co-signer that attests the off-chain result
    #[account(address = config.outcome_authority)]
    pub outcome_authority: Signer<'info>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
#[instruction(session_id: u64)]
pub struct MintEndingNft<'info> {
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(init, payer = player, space = 8 + NftReceipt::LEN, seeds = [b"nft", player.key().as_ref(), &session_id.to_le_bytes()], bump)]
    pub receipt: Account<'info, NftReceipt>,
    #[account(mut, token::mint = config.mint, token::authority = player)]
    pub player_ata: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, address = config.mint)]
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mut)]
    pub player: Signer<'info>,
    /// the backend co-signer that attests the ending
    #[account(address = config.outcome_authority)]
    pub outcome_authority: Signer<'info>,
    /// the new Core asset (ephemeral keypair, signs its own creation)
    #[account(mut)]
    pub asset: Signer<'info>,
    /// CHECK: Metaplex Core program, validated by address
    #[account(address = MPL_CORE_ID)]
    pub mpl_core: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}
