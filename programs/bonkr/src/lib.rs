use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer, MintTo, Burn};
use anchor_spl::associated_token::AssociatedToken;

declare_id!("14cdFgoduHhJQtheRPn3GF48YLR89jMcucdpkJKgsq4w");

pub const TOTAL_SUPPLY: u64 = 1_000_000_000 * 1_000_000_000;
pub const INITIAL_VIRTUAL_SOL: u64 = 30 * 1_000_000_000;
pub const INITIAL_VIRTUAL_TOKENS: u64 = 1_073_000_000 * 1_000_000_000;
pub const GRADUATION_USD: u64 = 30_000 * 1_000_000;
pub const BURN_FEE_BP: u64 = 50;
pub const PLATFORM_FEE_BP: u64 = 100;
pub const CREATOR_FEE_BP: u64 = 50;
pub const TOTAL_FEE_BP: u64 = 200;
pub const BP_DENOMINATOR: u64 = 10_000;

#[program]
pub mod bonkr {
    use super::*;

    pub fn initialize(
        ctx: Context<Initialize>,
        creation_fee: u64,
    ) -> Result<()> {
        let config = &mut ctx.accounts.config;
        config.authority = ctx.accounts.authority.key();
        config.platform_fee_recipient = ctx.accounts.platform_fee_recipient.key();
        config.oracle = ctx.accounts.authority.key();
        config.creation_fee = creation_fee;
        config.token_count = 0;
        config.is_paused = false;
        config.sol_price_usd = 200 * 1_000_000;
        config.bump = ctx.bumps.config;
        Ok(())
    }

    pub fn update_sol_price(ctx: Context<UpdateConfig>, price_usd: u64) -> Result<()> {
        require!(price_usd > 0, BonkrError::InvalidAmount);
        ctx.accounts.config.sol_price_usd = price_usd;
        Ok(())
    }

    pub fn set_oracle(ctx: Context<UpdateConfig>, oracle: Pubkey) -> Result<()> {
        ctx.accounts.config.oracle = oracle;
        Ok(())
    }

    pub fn create_token(
        ctx: Context<CreateToken>,
        name: String,
        symbol: String,
        uri: String,
        initial_buy_sol: u64,
    ) -> Result<()> {
        require!(!ctx.accounts.config.is_paused, BonkrError::FactoryPaused);
        require!(name.len() <= 32, BonkrError::NameTooLong);
        require!(symbol.len() <= 10, BonkrError::SymbolTooLong);

        let creation_fee = ctx.accounts.config.creation_fee;
        if creation_fee > 0 {
            let cpi_context = CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                anchor_lang::system_program::Transfer {
                    from: ctx.accounts.creator.to_account_info(),
                    to: ctx.accounts.platform_fee_recipient.to_account_info(),
                },
            );
            anchor_lang::system_program::transfer(cpi_context, creation_fee)?;
        }

        let token_state = &mut ctx.accounts.token_state;
        token_state.mint = ctx.accounts.mint.key();
        token_state.creator = ctx.accounts.creator.key();
        token_state.name = name;
        token_state.symbol = symbol;
        token_state.uri = uri;
        token_state.virtual_sol_reserve = INITIAL_VIRTUAL_SOL;
        token_state.virtual_token_reserve = INITIAL_VIRTUAL_TOKENS;
        token_state.real_sol_reserve = 0;
        token_state.real_token_reserve = TOTAL_SUPPLY;
        token_state.total_burned = 0;
        token_state.volume = 0;
        token_state.creator_fees_accrued = 0;
        token_state.is_graduated = false;
        token_state.is_paused = false;
        token_state.created_at = Clock::get()?.unix_timestamp;
        token_state.bump = ctx.bumps.token_state;
        token_state.vault_bump = ctx.bumps.sol_vault;

        let seeds = &[
            b"token_state",
            ctx.accounts.mint.to_account_info().key.as_ref(),
            &[token_state.bump],
        ];
        let signer_seeds = &[&seeds[..]];

        let cpi_accounts = MintTo {
            mint: ctx.accounts.mint.to_account_info(),
            to: ctx.accounts.token_vault.to_account_info(),
            authority: ctx.accounts.token_state.to_account_info(),
        };
        let cpi_program = ctx.accounts.token_program.to_account_info();
        let cpi_ctx = CpiContext::new_with_signer(cpi_program, cpi_accounts, signer_seeds);
        token::mint_to(cpi_ctx, TOTAL_SUPPLY)?;

        ctx.accounts.config.token_count += 1;

        if initial_buy_sol > 0 {
            execute_buy_internal(
                token_state,
                &ctx.accounts.creator,
                &ctx.accounts.creator_token_account,
                &ctx.accounts.token_vault,
                &ctx.accounts.sol_vault,
                &ctx.accounts.platform_fee_recipient,
                &ctx.accounts.token_program,
                &ctx.accounts.system_program,
                initial_buy_sol,
                0,
            )?;
        }

        emit!(TokenCreated {
            mint: ctx.accounts.mint.key(),
            creator: ctx.accounts.creator.key(),
            name: token_state.name.clone(),
            symbol: token_state.symbol.clone(),
            initial_buy_sol,
        });

        Ok(())
    }

    pub fn buy(
        ctx: Context<Trade>,
        sol_amount: u64,
        min_tokens_out: u64,
    ) -> Result<()> {
        require!(!ctx.accounts.config.is_paused, BonkrError::FactoryPaused);
        require!(!ctx.accounts.token_state.is_paused, BonkrError::TokenPaused);
        require!(!ctx.accounts.token_state.is_graduated, BonkrError::TokenGraduated);
        require!(sol_amount > 0, BonkrError::InvalidAmount);

        execute_buy_internal(
            &mut ctx.accounts.token_state,
            &ctx.accounts.user,
            &ctx.accounts.user_token_account,
            &ctx.accounts.token_vault,
            &ctx.accounts.sol_vault,
            &ctx.accounts.platform_fee_recipient,
            &ctx.accounts.token_program,
            &ctx.accounts.system_program,
            sol_amount,
            min_tokens_out,
        )?;

        check_graduation(&mut ctx.accounts.token_state, ctx.accounts.config.sol_price_usd)?;

        Ok(())
    }

    pub fn sell(
        ctx: Context<Trade>,
        token_amount: u64,
        min_sol_out: u64,
    ) -> Result<()> {
        require!(!ctx.accounts.config.is_paused, BonkrError::FactoryPaused);
        require!(!ctx.accounts.token_state.is_paused, BonkrError::TokenPaused);
        require!(!ctx.accounts.token_state.is_graduated, BonkrError::TokenGraduated);
        require!(token_amount > 0, BonkrError::InvalidAmount);

        let token_state = &mut ctx.accounts.token_state;

        let k = (token_state.virtual_sol_reserve as u128) * (token_state.virtual_token_reserve as u128);
        let new_token_reserve = token_state.virtual_token_reserve + token_amount;
        let new_sol_reserve = (k / new_token_reserve as u128) as u64;
        let sol_out_gross = token_state.virtual_sol_reserve - new_sol_reserve;

        require!(sol_out_gross <= token_state.real_sol_reserve, BonkrError::InsufficientLiquidity);

        let platform_fee = (sol_out_gross * PLATFORM_FEE_BP) / BP_DENOMINATOR;
        let creator_fee = (sol_out_gross * CREATOR_FEE_BP) / BP_DENOMINATOR;
        let burn_fee_tokens = (token_amount * BURN_FEE_BP) / BP_DENOMINATOR;
        let sol_to_seller = sol_out_gross - (sol_out_gross * TOTAL_FEE_BP) / BP_DENOMINATOR;

        require!(sol_to_seller >= min_sol_out, BonkrError::SlippageExceeded);

        let cpi_accounts = Transfer {
            from: ctx.accounts.user_token_account.to_account_info(),
            to: ctx.accounts.token_vault.to_account_info(),
            authority: ctx.accounts.user.to_account_info(),
        };
        let cpi_program = ctx.accounts.token_program.to_account_info();
        token::transfer(CpiContext::new(cpi_program, cpi_accounts), token_amount)?;

        if burn_fee_tokens > 0 {
            let seeds = &[
                b"token_state",
                token_state.mint.as_ref(),
                &[token_state.bump],
            ];
            let signer_seeds = &[&seeds[..]];

            let cpi_accounts = Burn {
                mint: ctx.accounts.mint.to_account_info(),
                from: ctx.accounts.token_vault.to_account_info(),
                authority: token_state.to_account_info(),
            };
            let cpi_program = ctx.accounts.token_program.to_account_info();
            token::burn(CpiContext::new_with_signer(cpi_program, cpi_accounts, signer_seeds), burn_fee_tokens)?;
            token_state.total_burned += burn_fee_tokens;
        }

        token_state.virtual_sol_reserve = new_sol_reserve;
        token_state.virtual_token_reserve = new_token_reserve;
        token_state.real_sol_reserve -= sol_out_gross;
        token_state.real_token_reserve += token_amount - burn_fee_tokens;
        token_state.volume += sol_out_gross;
        token_state.creator_fees_accrued += creator_fee;

        **ctx.accounts.sol_vault.to_account_info().try_borrow_mut_lamports()? -= platform_fee;
        **ctx.accounts.platform_fee_recipient.to_account_info().try_borrow_mut_lamports()? += platform_fee;

        **ctx.accounts.sol_vault.to_account_info().try_borrow_mut_lamports()? -= sol_to_seller;
        **ctx.accounts.user.to_account_info().try_borrow_mut_lamports()? += sol_to_seller;

        emit!(TokenSold {
            mint: token_state.mint,
            seller: ctx.accounts.user.key(),
            token_amount,
            sol_amount: sol_to_seller,
        });

        check_graduation(token_state, ctx.accounts.config.sol_price_usd)?;

        Ok(())
    }

    pub fn claim_creator_fees(ctx: Context<ClaimCreatorFees>) -> Result<()> {
        let token_state = &ctx.accounts.token_state;
        let amount = token_state.creator_fees_accrued;
        require!(amount > 0, BonkrError::NoFeesToClaim);
        require!(ctx.accounts.creator.key() == token_state.creator, BonkrError::NotCreator);

        let token_state_mut = &mut ctx.accounts.token_state.clone();
        
        **ctx.accounts.sol_vault.to_account_info().try_borrow_mut_lamports()? -= amount;
        **ctx.accounts.creator.to_account_info().try_borrow_mut_lamports()? += amount;

        emit!(CreatorFeesClaimed {
            creator: ctx.accounts.creator.key(),
            amount,
        });

        Ok(())
    }

    pub fn admin_withdraw_lp(ctx: Context<AdminWithdrawLP>) -> Result<()> {
        let token_state = &mut ctx.accounts.token_state;
        
        let sol_amount = token_state.real_sol_reserve;
        let token_amount = token_state.real_token_reserve;

        token_state.real_sol_reserve = 0;
        token_state.real_token_reserve = 0;
        token_state.is_paused = true;

        if sol_amount > 0 {
            **ctx.accounts.sol_vault.to_account_info().try_borrow_mut_lamports()? -= sol_amount;
            **ctx.accounts.recipient.to_account_info().try_borrow_mut_lamports()? += sol_amount;
        }

        if token_amount > 0 {
            let seeds = &[
                b"token_state",
                token_state.mint.as_ref(),
                &[token_state.bump],
            ];
            let signer_seeds = &[&seeds[..]];

            let cpi_accounts = Transfer {
                from: ctx.accounts.token_vault.to_account_info(),
                to: ctx.accounts.recipient_token_account.to_account_info(),
                authority: token_state.to_account_info(),
            };
            let cpi_program = ctx.accounts.token_program.to_account_info();
            token::transfer(CpiContext::new_with_signer(cpi_program, cpi_accounts, signer_seeds), token_amount)?;
        }

        emit!(LPWithdrawn {
            mint: token_state.mint,
            sol_amount,
            token_amount,
            recipient: ctx.accounts.recipient.key(),
        });

        Ok(())
    }

    pub fn force_graduate(ctx: Context<AdminAction>) -> Result<()> {
        let token_state = &mut ctx.accounts.token_state;
        require!(!token_state.is_graduated, BonkrError::AlreadyGraduated);
        token_state.is_graduated = true;

        emit!(TokenGraduated {
            mint: token_state.mint,
            market_cap_usd: 0,
        });

        Ok(())
    }

    pub fn pause_token(ctx: Context<AdminAction>, paused: bool) -> Result<()> {
        ctx.accounts.token_state.is_paused = paused;
        Ok(())
    }

    pub fn pause_factory(ctx: Context<UpdateConfig>, paused: bool) -> Result<()> {
        ctx.accounts.config.is_paused = paused;
        Ok(())
    }

    pub fn set_creation_fee(ctx: Context<UpdateConfig>, fee: u64) -> Result<()> {
        ctx.accounts.config.creation_fee = fee;
        Ok(())
    }

    pub fn set_platform_fee_recipient(ctx: Context<UpdateConfig>, recipient: Pubkey) -> Result<()> {
        ctx.accounts.config.platform_fee_recipient = recipient;
        Ok(())
    }

    pub fn emergency_withdraw(ctx: Context<EmergencyWithdraw>) -> Result<()> {
        let balance = ctx.accounts.sol_vault.to_account_info().lamports();
        let rent = Rent::get()?.minimum_balance(0);
        let withdrawable = balance.saturating_sub(rent);
        
        require!(withdrawable > 0, BonkrError::NoFundsToWithdraw);

        **ctx.accounts.sol_vault.to_account_info().try_borrow_mut_lamports()? -= withdrawable;
        **ctx.accounts.authority.to_account_info().try_borrow_mut_lamports()? += withdrawable;

        emit!(EmergencyWithdrawEvent { amount: withdrawable });

        Ok(())
    }
}

fn execute_buy_internal<'info>(
    token_state: &mut Account<'info, TokenState>,
    user: &Signer<'info>,
    user_token_account: &Account<'info, TokenAccount>,
    token_vault: &Account<'info, TokenAccount>,
    sol_vault: &AccountInfo<'info>,
    platform_fee_recipient: &AccountInfo<'info>,
    token_program: &Program<'info, Token>,
    system_program: &Program<'info, anchor_lang::system_program::System>,
    sol_amount: u64,
    min_tokens_out: u64,
) -> Result<()> {
    let platform_fee = (sol_amount * PLATFORM_FEE_BP) / BP_DENOMINATOR;
    let creator_fee = (sol_amount * CREATOR_FEE_BP) / BP_DENOMINATOR;
    let sol_to_reserve = sol_amount - platform_fee - creator_fee;

    let k = (token_state.virtual_sol_reserve as u128) * (token_state.virtual_token_reserve as u128);
    let new_sol_reserve = token_state.virtual_sol_reserve + sol_to_reserve;
    let new_token_reserve = (k / new_sol_reserve as u128) as u64;
    let tokens_out = token_state.virtual_token_reserve - new_token_reserve;

    require!(tokens_out >= min_tokens_out, BonkrError::SlippageExceeded);
    require!(tokens_out <= token_state.real_token_reserve, BonkrError::InsufficientTokens);

    let burn_amount = (tokens_out * BURN_FEE_BP) / BP_DENOMINATOR;
    let tokens_to_buyer = tokens_out - burn_amount;

    let cpi_context = CpiContext::new(
        system_program.to_account_info(),
        anchor_lang::system_program::Transfer {
            from: user.to_account_info(),
            to: sol_vault.to_account_info(),
        },
    );
    anchor_lang::system_program::transfer(cpi_context, sol_to_reserve + creator_fee)?;

    let cpi_context = CpiContext::new(
        system_program.to_account_info(),
        anchor_lang::system_program::Transfer {
            from: user.to_account_info(),
            to: platform_fee_recipient.to_account_info(),
        },
    );
    anchor_lang::system_program::transfer(cpi_context, platform_fee)?;

    let seeds = &[
        b"token_state",
        token_state.mint.as_ref(),
        &[token_state.bump],
    ];
    let signer_seeds = &[&seeds[..]];

    let cpi_accounts = Transfer {
        from: token_vault.to_account_info(),
        to: user_token_account.to_account_info(),
        authority: token_state.to_account_info(),
    };
    let cpi_program = token_program.to_account_info();
    token::transfer(CpiContext::new_with_signer(cpi_program, cpi_accounts, signer_seeds), tokens_to_buyer)?;

    token_state.virtual_sol_reserve = new_sol_reserve;
    token_state.virtual_token_reserve = new_token_reserve;
    token_state.real_sol_reserve += sol_to_reserve + creator_fee;
    token_state.real_token_reserve -= tokens_out;
    token_state.total_burned += burn_amount;
    token_state.volume += sol_amount;
    token_state.creator_fees_accrued += creator_fee;

    emit!(TokenBought {
        mint: token_state.mint,
        buyer: user.key(),
        sol_amount,
        token_amount: tokens_to_buyer,
    });

    Ok(())
}

fn check_graduation(token_state: &mut Account<TokenState>, sol_price_usd: u64) -> Result<()> {
    if token_state.is_graduated {
        return Ok(());
    }

    let market_cap_sol = (token_state.virtual_sol_reserve as u128 * TOTAL_SUPPLY as u128) 
        / token_state.virtual_token_reserve as u128;
    
    let market_cap_usd = (market_cap_sol * sol_price_usd as u128) / 1_000_000_000;

    if market_cap_usd >= GRADUATION_USD as u128 {
        token_state.is_graduated = true;
        emit!(TokenGraduated {
            mint: token_state.mint,
            market_cap_usd: market_cap_usd as u64,
        });
    }

    Ok(())
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = authority,
        space = 8 + GlobalConfig::INIT_SPACE,
        seeds = [b"config"],
        bump
    )]
    pub config: Account<'info, GlobalConfig>,
    #[account(mut)]
    pub authority: Signer<'info>,
    pub platform_fee_recipient: SystemAccount<'info>,
    pub system_program: Program<'info, anchor_lang::system_program::System>,
}

#[derive(Accounts)]
#[instruction(name: String, symbol: String)]
pub struct CreateToken<'info> {
    #[account(
        mut,
        seeds = [b"config"],
        bump = config.bump
    )]
    pub config: Account<'info, GlobalConfig>,
    
    #[account(
        init,
        payer = creator,
        mint::decimals = 9,
        mint::authority = token_state,
    )]
    pub mint: Account<'info, Mint>,
    
    #[account(
        init,
        payer = creator,
        space = 8 + TokenState::INIT_SPACE,
        seeds = [b"token_state", mint.key().as_ref()],
        bump
    )]
    pub token_state: Account<'info, TokenState>,
    
    #[account(
        init,
        payer = creator,
        associated_token::mint = mint,
        associated_token::authority = token_state,
    )]
    pub token_vault: Account<'info, TokenAccount>,
    
    #[account(
        mut,
        seeds = [b"sol_vault", mint.key().as_ref()],
        bump
    )]
    pub sol_vault: SystemAccount<'info>,
    
    #[account(
        init_if_needed,
        payer = creator,
        associated_token::mint = mint,
        associated_token::authority = creator,
    )]
    pub creator_token_account: Account<'info, TokenAccount>,
    
    #[account(mut)]
    pub creator: Signer<'info>,
    
    #[account(mut, address = config.platform_fee_recipient)]
    pub platform_fee_recipient: SystemAccount<'info>,
    
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, anchor_lang::system_program::System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Trade<'info> {
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, GlobalConfig>,
    
    pub mint: Account<'info, Mint>,
    
    #[account(
        mut,
        seeds = [b"token_state", mint.key().as_ref()],
        bump = token_state.bump
    )]
    pub token_state: Account<'info, TokenState>,
    
    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = token_state,
    )]
    pub token_vault: Account<'info, TokenAccount>,
    
    #[account(
        mut,
        seeds = [b"sol_vault", mint.key().as_ref()],
        bump = token_state.vault_bump
    )]
    pub sol_vault: SystemAccount<'info>,
    
    #[account(
        init_if_needed,
        payer = user,
        associated_token::mint = mint,
        associated_token::authority = user,
    )]
    pub user_token_account: Account<'info, TokenAccount>,
    
    #[account(mut)]
    pub user: Signer<'info>,
    
    #[account(mut, address = config.platform_fee_recipient)]
    pub platform_fee_recipient: SystemAccount<'info>,
    
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, anchor_lang::system_program::System>,
}

#[derive(Accounts)]
pub struct ClaimCreatorFees<'info> {
    #[account(
        mut,
        seeds = [b"token_state", token_state.mint.as_ref()],
        bump = token_state.bump
    )]
    pub token_state: Account<'info, TokenState>,
    
    #[account(
        mut,
        seeds = [b"sol_vault", token_state.mint.as_ref()],
        bump = token_state.vault_bump
    )]
    pub sol_vault: SystemAccount<'info>,
    
    #[account(mut)]
    pub creator: Signer<'info>,
    
    pub system_program: Program<'info, anchor_lang::system_program::System>,
}

#[derive(Accounts)]
pub struct AdminWithdrawLP<'info> {
    #[account(
        seeds = [b"config"],
        bump = config.bump,
        has_one = authority
    )]
    pub config: Account<'info, GlobalConfig>,
    
    pub mint: Account<'info, Mint>,
    
    #[account(
        mut,
        seeds = [b"token_state", mint.key().as_ref()],
        bump = token_state.bump
    )]
    pub token_state: Account<'info, TokenState>,
    
    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = token_state,
    )]
    pub token_vault: Account<'info, TokenAccount>,
    
    #[account(
        mut,
        seeds = [b"sol_vault", mint.key().as_ref()],
        bump = token_state.vault_bump
    )]
    pub sol_vault: SystemAccount<'info>,
    
    #[account(
        init_if_needed,
        payer = authority,
        associated_token::mint = mint,
        associated_token::authority = recipient,
    )]
    pub recipient_token_account: Account<'info, TokenAccount>,
    
    #[account(mut)]
    pub recipient: SystemAccount<'info>,
    
    #[account(mut)]
    pub authority: Signer<'info>,
    
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, anchor_lang::system_program::System>,
}

#[derive(Accounts)]
pub struct AdminAction<'info> {
    #[account(
        seeds = [b"config"],
        bump = config.bump,
        has_one = authority
    )]
    pub config: Account<'info, GlobalConfig>,
    
    #[account(
        mut,
        seeds = [b"token_state", token_state.mint.as_ref()],
        bump = token_state.bump
    )]
    pub token_state: Account<'info, TokenState>,
    
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct UpdateConfig<'info> {
    #[account(
        mut,
        seeds = [b"config"],
        bump = config.bump,
        has_one = authority
    )]
    pub config: Account<'info, GlobalConfig>,
    
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct EmergencyWithdraw<'info> {
    #[account(
        seeds = [b"config"],
        bump = config.bump,
        has_one = authority
    )]
    pub config: Account<'info, GlobalConfig>,
    
    #[account(mut)]
    pub sol_vault: SystemAccount<'info>,
    
    #[account(mut)]
    pub authority: Signer<'info>,
    
    pub system_program: Program<'info, anchor_lang::system_program::System>,
}

#[account]
#[derive(InitSpace)]
pub struct GlobalConfig {
    pub authority: Pubkey,
    pub platform_fee_recipient: Pubkey,
    pub oracle: Pubkey,
    pub creation_fee: u64,
    pub token_count: u64,
    pub sol_price_usd: u64,
    pub is_paused: bool,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct TokenState {
    pub mint: Pubkey,
    pub creator: Pubkey,
    #[max_len(32)]
    pub name: String,
    #[max_len(10)]
    pub symbol: String,
    #[max_len(200)]
    pub uri: String,
    pub virtual_sol_reserve: u64,
    pub virtual_token_reserve: u64,
    pub real_sol_reserve: u64,
    pub real_token_reserve: u64,
    pub total_burned: u64,
    pub volume: u64,
    pub creator_fees_accrued: u64,
    pub is_graduated: bool,
    pub is_paused: bool,
    pub created_at: i64,
    pub bump: u8,
    pub vault_bump: u8,
}

#[event]
pub struct TokenCreated {
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub name: String,
    pub symbol: String,
    pub initial_buy_sol: u64,
}

#[event]
pub struct TokenBought {
    pub mint: Pubkey,
    pub buyer: Pubkey,
    pub sol_amount: u64,
    pub token_amount: u64,
}

#[event]
pub struct TokenSold {
    pub mint: Pubkey,
    pub seller: Pubkey,
    pub token_amount: u64,
    pub sol_amount: u64,
}

#[event]
pub struct TokenGraduated {
    pub mint: Pubkey,
    pub market_cap_usd: u64,
}

#[event]
pub struct CreatorFeesClaimed {
    pub creator: Pubkey,
    pub amount: u64,
}

#[event]
pub struct LPWithdrawn {
    pub mint: Pubkey,
    pub sol_amount: u64,
    pub token_amount: u64,
    pub recipient: Pubkey,
}

#[event]
pub struct EmergencyWithdrawEvent {
    pub amount: u64,
}

#[error_code]
pub enum BonkrError {
    #[msg("Factory is paused")]
    FactoryPaused,
    #[msg("Token is paused")]
    TokenPaused,
    #[msg("Token has graduated")]
    TokenGraduated,
    #[msg("Already graduated")]
    AlreadyGraduated,
    #[msg("Invalid amount")]
    InvalidAmount,
    #[msg("Slippage exceeded")]
    SlippageExceeded,
    #[msg("Insufficient tokens in reserve")]
    InsufficientTokens,
    #[msg("Insufficient liquidity")]
    InsufficientLiquidity,
    #[msg("No fees to claim")]
    NoFeesToClaim,
    #[msg("No funds to withdraw")]
    NoFundsToWithdraw,
    #[msg("Name too long (max 32 chars)")]
    NameTooLong,
    #[msg("Symbol too long (max 10 chars)")]
    SymbolTooLong,
    #[msg("Not the token creator")]
    NotCreator,
}
