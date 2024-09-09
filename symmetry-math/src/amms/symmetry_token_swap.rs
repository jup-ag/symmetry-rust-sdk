use anchor_lang::accounts::sysvar;
use anchor_lang::prelude::AccountMeta;
use anyhow::{anyhow, Context, Error, Result};

use rust_decimal::Decimal;
use solana_sdk::sysvar::clock::{self, Clock};
use solana_sdk::{instruction::Instruction, pubkey, pubkey::Pubkey};

use jupiter_amm_interface::{
    try_get_account_data, AccountMap, Amm, KeyedAccount, Quote, QuoteParams, SwapAndAccountMetas,
    SwapParams,
};
use jupiter_amm_interface::{AmmContext, Swap};

use super::accounts::mul_div;
use crate::amms::accounts::{
    CurveData, FundState, OraclePrice, TokenList, TokenPriceData, TokenSettings,
};
use crate::amms::accounts::{
    BPS_DIVIDER, LP_DISABLED, MAX_TOKENS_IN_ASSET_POOL, NUM_OF_POINTS_IN_CURVE_DATA,
    USE_CURVE_DATA, WEIGHT_MULTIPLIER,
};

// struct SymmetryTokenSwap {
//     key: Pubkey,
//     label: String,
//     fund_state: FundState,
//     token_list: Option<TokenList>,
//     fund_worth: Option<u64>,
//     curve_data: CurveData,
//     program_id: Pubkey,
//     clock: Option<Clock>,
// }

pub const SYMMETRY_PROGRAM_ADDRESS: Pubkey =
    pubkey!("2KehYt3KsEQR53jYcxjbQp2d2kCp4AkuQW68atufRwSr");
pub const TOKEN_LIST_ADDRESS: Pubkey = pubkey!("3SnUughtueoVrhevXTLMf586qvKNNXggNsc7NgoMUU1t");
pub const CURVE_DATA_ADDRESS: Pubkey = pubkey!("4QMjSHuM3iS7Fdfi8kZJfHRKoEJSDHEtEwqbChsTcUVK");
pub const PDA_ADDRESS: Pubkey = pubkey!("BLBYiq48WcLQ5SxiftyKmPtmsZPUBEnDEjqEnKGAR4zx");
pub const SWAP_FEE_ADDRESS: Pubkey = pubkey!("AWfpfzA6FYbqx4JLz75PDgsjH7jtBnnmJ6MXW5zNY2Ei");
pub const SYMMETRY_PROGRAM_SWAP_INSTRUCTION_ID: u64 = 219478785678209410;

pub struct SymmetryMath {}
impl SymmetryMath {
    pub fn mul_div(a: u64, b: u64, c: u64) -> Option<u64> {
        mul_div(a, b, c)
    }

    pub fn amount_to_usd_value(amount: u64, decimals: u8, price: u64) -> Result<u64> {
        SymmetryMath::mul_div(amount, price, u64::pow(10, decimals as u32)).context("mul div err")
    }

    pub fn usd_value_to_amount(worth: u64, decimals: u8, price: u64) -> Result<u64> {
        SymmetryMath::mul_div(worth, u64::pow(10, decimals as u32), price).context("mul div err")
    }

    pub fn compute_value_of_sold_token(
        amount: u64,
        token_settings: TokenSettings,
        price: OraclePrice,
        start_amount: u64,
        target_amount: u64,
        curve_data: TokenPriceData,
    ) -> Result<u64> {
        let mut current_amount = start_amount;
        let mut curve_offset = if start_amount > target_amount {
            start_amount - target_amount
        } else {
            0
        };
        let mut current_output_value: u64 = 0;
        let mut amount_left: u64 = amount;
        let mut current_price = price.sell_price;

        for step in 0..NUM_OF_POINTS_IN_CURVE_DATA + 1 {
            let step_amount = if step < NUM_OF_POINTS_IN_CURVE_DATA {
                curve_data.amount[step]
            } else {
                amount_left
            };
            if step < NUM_OF_POINTS_IN_CURVE_DATA && curve_data.price[step] < current_price {
                if token_settings.use_curve_data == USE_CURVE_DATA {
                    current_price = curve_data.price[step];
                }
            }
            if step == NUM_OF_POINTS_IN_CURVE_DATA {
                curve_offset = 0;
            }
            if step_amount <= curve_offset {
                curve_offset -= step_amount;
                continue;
            }
            let mut amount_in_interval = step_amount - curve_offset;
            curve_offset = 0;
            if amount_in_interval > amount_left {
                amount_in_interval = amount_left
            };
            let mut amount_before_tw = amount_in_interval;
            if current_amount >= target_amount {
                amount_before_tw = 0;
            } else if current_amount + amount_in_interval >= target_amount {
                amount_before_tw -= current_amount + amount_in_interval - target_amount;
            }
            let amount_after_tw = amount_in_interval - amount_before_tw;
            let value_before_tw = SymmetryMath::amount_to_usd_value(
                amount_before_tw,
                token_settings.decimals,
                current_price,
            )?;
            let value_after_tw = SymmetryMath::amount_to_usd_value(
                amount_after_tw,
                token_settings.decimals,
                current_price,
            )?;
            let fees = SymmetryMath::mul_div(
                value_before_tw,
                token_settings.token_swap_fee_before_tw_bps as u64,
                BPS_DIVIDER,
            )
            .context("mul div err")?
                + SymmetryMath::mul_div(
                    value_after_tw,
                    token_settings.token_swap_fee_after_tw_bps as u64,
                    BPS_DIVIDER,
                )
                .context("mul div err")?;
            current_output_value += value_before_tw + value_after_tw - fees;
            amount_left -= amount_in_interval;
            current_amount += amount_in_interval;
            if amount_left == 0 {
                break;
            }
        }

        Ok(current_output_value)
    }

    pub fn compute_amount_of_bought_token(
        value: u64,
        token_settings: TokenSettings,
        price: OraclePrice,
        start_amount: u64,
        target_amount: u64,
        curve_data: TokenPriceData,
    ) -> Result<u64> {
        let mut current_amount = start_amount;
        let mut curve_offset = if start_amount < target_amount {
            target_amount - start_amount
        } else {
            0
        };
        let mut current_output_amount: u64 = 0;
        let mut value_left: u64 = value;
        let mut current_price = price.buy_price;

        for step in 0..NUM_OF_POINTS_IN_CURVE_DATA + 1 {
            let step_amount = if step < NUM_OF_POINTS_IN_CURVE_DATA {
                curve_data.amount[step]
            } else {
                SymmetryMath::usd_value_to_amount(
                    value_left * 2,
                    token_settings.decimals,
                    current_price,
                )?
            };
            if step < NUM_OF_POINTS_IN_CURVE_DATA && curve_data.price[step] > current_price {
                if token_settings.use_curve_data == USE_CURVE_DATA {
                    current_price = curve_data.price[step];
                };
            }
            if step == NUM_OF_POINTS_IN_CURVE_DATA {
                curve_offset = 0;
            }
            if step_amount <= curve_offset {
                curve_offset -= step_amount;
                continue;
            }
            let mut amount_in_interval = step_amount - curve_offset;
            curve_offset = 0;

            let mut value_in_interval = SymmetryMath::amount_to_usd_value(
                amount_in_interval,
                token_settings.decimals,
                current_price,
            )?;
            if value_in_interval > value_left {
                value_in_interval = value_left;
                amount_in_interval = SymmetryMath::usd_value_to_amount(
                    value_in_interval,
                    token_settings.decimals,
                    current_price,
                )?;
            }

            let mut value_before_tw = value_in_interval;
            if current_amount <= target_amount {
                value_before_tw = 0;
            } else if current_amount <= target_amount + amount_in_interval {
                value_before_tw -= SymmetryMath::amount_to_usd_value(
                    target_amount + amount_in_interval - current_amount,
                    token_settings.decimals,
                    current_price,
                )?
            }
            let value_after_tw = value_in_interval - value_before_tw;

            let fees = SymmetryMath::mul_div(
                value_before_tw,
                token_settings.token_swap_fee_before_tw_bps as u64,
                BPS_DIVIDER,
            )
            .context("mul div err")?
                + SymmetryMath::mul_div(
                    value_after_tw,
                    token_settings.token_swap_fee_after_tw_bps as u64,
                    BPS_DIVIDER,
                )
                .context("mul div err")?;

            let amount_bought = SymmetryMath::usd_value_to_amount(
                value_in_interval - fees,
                token_settings.decimals,
                current_price,
            )?;

            current_output_amount += amount_bought;
            value_left -= value_in_interval;
            if amount_bought > current_amount {
                current_amount = 0;
            } else {
                current_amount -= amount_bought;
            }
            if value_left == 0 {
                break;
            }
        }

        Ok(current_output_amount)
    }
}

// impl Amm for SymmetryTokenSwap {
//     fn from_keyed_account(keyed_account: &KeyedAccount, amm_context: &AmmContext) -> Result<Self> {
//         // pub fn from_keyed_account(
//         //     fund_state_account: &KeyedAccount,
//         //     token_list_account: &KeyedAccount,
//         // ) -> Result<Self> {
//         //     let fund_state_loader = FundState::load(&fund_state_account.account.data);
//         //     if let Err(e) = fund_state_loader {
//         //         return Err(e);
//         //     }
//         //     let fund_state = fund_state_loader.unwrap();
//         //     let token_list_loader = TokenList::load(&token_list_account.account.data);
//         //     if let Err(e) = token_list_loader {
//         //         return Err(e);
//         //     }
//         //     let token_list = token_list_loader.unwrap();

//         //     Ok(Self {
//         //         key: fund_state_account.key,
//         //         label: String::from("Symmetry"),
//         //         fund_state: fund_state,
//         //         token_list: token_list,
//         //         curve_data: CurveData::empty(),
//         //         program_id: SymmetryTokenSwap::SYMMETRY_PROGRAM_ADDRESS,
//         //     })
//         // }
//         todo!("ignore")
//     }

//     fn label(&self) -> String {
//         self.label.clone()
//     }

//     fn program_id(&self) -> Pubkey {
//         self.program_id
//     }

//     fn key(&self) -> Pubkey {
//         self.key
//     }

//     fn get_reserve_mints(&self) -> Vec<Pubkey> {
//         let mut vec: Vec<Pubkey> = Vec::new();
//         for i in 0..self.fund_state.num_of_tokens as usize {
//             if let Some(token_list) = self.token_list {
//                 if token_list.list[self.fund_state.current_comp_token[i] as usize].lp_on
//                     != LP_DISABLED
//                 {
//                     vec.push(
//                         token_list.list[self.fund_state.current_comp_token[i] as usize].token_mint,
//                     )
//                 }
//             }
//         }
//         return vec;
//     }

//     fn get_accounts_to_update(&self) -> Vec<Pubkey> {
//         let mut accounts_to_update = Vec::with_capacity(4 + self.fund_state.num_of_tokens as usize);
//         accounts_to_update.extend([self.key, TOKEN_LIST_ADDRESS, CURVE_DATA_ADDRESS, clock::ID]);

//         if let Some(token_list) = self.token_list {
//             for i in 0..self.fund_state.num_of_tokens as usize {
//                 accounts_to_update.push(
//                     token_list.list[self.fund_state.current_comp_token[i] as usize].oracle_account,
//                 );
//             }
//         }

//         accounts_to_update
//     }

//     fn update(&mut self, account_map: &AccountMap) -> Result<()> {
//         self.token_list = None;
//         let mut token_list =
//             TokenList::load(try_get_account_data(account_map, &TOKEN_LIST_ADDRESS)?)?;
//         self.curve_data = CurveData::load(try_get_account_data(account_map, &CURVE_DATA_ADDRESS)?)?;

//         let clock: Clock = bincode::deserialize(try_get_account_data(account_map, &clock::ID)?)?;

//         let fund_state = FundState::load(try_get_account_data(account_map, &self.key)?)?;
//         self.fund_worth = None;

//         let mut fund_worth = 0;
//         for i in 0..self.fund_state.num_of_tokens as usize {
//             let token_settings = token_list.list[fund_state.current_comp_token[i] as usize];
//             let oracle_account = &token_settings.oracle_account;
//             let oracle_price = OraclePrice::load(
//                 try_get_account_data(account_map, oracle_account)?,
//                 token_settings,
//                 clock.clone(),
//             )?;
//             token_list.list[fund_state.current_comp_token[i] as usize].oracle_price = oracle_price;

//             if oracle_price.oracle_live == 0 {
//                 return Err(Error::msg("One of the tokens has offline oracle status"));
//             }

//             fund_worth += SymmetryMath::amount_to_usd_value(
//                 fund_state.current_comp_amount[i],
//                 token_settings.decimals,
//                 oracle_price.avg_price,
//             )?;
//         }

//         self.fund_worth = Some(fund_worth);
//         self.token_list = Some(token_list);
//         self.clock = Some(clock);
//         // might need to set fund_state to None to avoid stale data
//         self.fund_state = fund_state;

//         Ok(())
//     }

//     fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
//         let fund_state = self.fund_state;
//         let token_list = self
//             .token_list
//             .ok_or_else(|| anyhow!("token_list is empty"))?;

//         let curve_data = self.curve_data;

//         let from_amount: u64 = quote_params.amount;
//         let from_token_id = token_list
//             .list
//             .iter()
//             .position(|&x| x.token_mint == quote_params.input_mint)
//             .context("fail to find from token id")?;
//         let to_token_id = token_list
//             .list
//             .iter()
//             .position(|&x| x.token_mint == quote_params.output_mint)
//             .context("fail to find to token id")?;

//         let from_token_settings = token_list.list[from_token_id as usize];
//         let to_token_settings = token_list.list[to_token_id as usize];

//         let from_token_index = fund_state
//             .current_comp_token
//             .iter()
//             .position(|&x| x == (from_token_id as u64))
//             .context("fail to find from token index")?;
//         let to_token_index = fund_state
//             .current_comp_token
//             .iter()
//             .position(|&x| x == (to_token_id as u64))
//             .context("fail to find to token index")?;

//         let mut fund_worth = self
//             .fund_worth
//             .ok_or_else(|| anyhow!("fund_worth is empty"))?;
//         let from_token_price = from_token_settings.oracle_price;
//         let to_token_price = to_token_settings.oracle_price;
//         println!("from_token_price: {:?}", from_token_price.avg_price);
//         println!("to_token_price: {:?}", to_token_price.avg_price);
//         let from_token_target_amount: u64 = SymmetryMath::usd_value_to_amount(
//             SymmetryMath::mul_div(
//                 fund_state.target_weight[from_token_index],
//                 fund_worth,
//                 fund_state.weight_sum,
//             )
//             .context("mul div err")?,
//             from_token_settings.decimals,
//             from_token_price.avg_price,
//         )?;
//         let to_token_target_amount: u64 = SymmetryMath::usd_value_to_amount(
//             SymmetryMath::mul_div(
//                 fund_state.target_weight[to_token_index],
//                 fund_worth,
//                 fund_state.weight_sum,
//             )
//             .context("mul div err")?,
//             to_token_settings.decimals,
//             to_token_price.avg_price,
//         )?;

//         let value = SymmetryMath::compute_value_of_sold_token(
//             from_amount,
//             from_token_settings,
//             from_token_price,
//             fund_state.current_comp_amount[from_token_index],
//             from_token_target_amount,
//             curve_data.sell[from_token_id as usize],
//         )?;

//         let mut to_amount = SymmetryMath::compute_amount_of_bought_token(
//             value,
//             to_token_settings,
//             to_token_price,
//             fund_state.current_comp_amount[to_token_index],
//             to_token_target_amount,
//             curve_data.buy[to_token_id as usize],
//         )?;

//         let mut amount_without_fees = SymmetryMath::usd_value_to_amount(
//             SymmetryMath::amount_to_usd_value(
//                 from_amount,
//                 from_token_settings.decimals,
//                 from_token_price.sell_price,
//             )?,
//             to_token_settings.decimals,
//             to_token_price.buy_price,
//         )?;

//         let fair_amount = SymmetryMath::usd_value_to_amount(
//             SymmetryMath::amount_to_usd_value(
//                 from_amount,
//                 from_token_settings.decimals,
//                 from_token_price.avg_price,
//             )?,
//             to_token_settings.decimals,
//             to_token_price.avg_price,
//         )?;

//         if amount_without_fees > fund_state.current_comp_amount[to_token_index] {
//             amount_without_fees = fund_state.current_comp_amount[to_token_index];
//         }

//         if to_amount > amount_without_fees {
//             to_amount = amount_without_fees
//         }

//         let total_fees = amount_without_fees - to_amount;

//         let symmetry_bps = token_list.list[0].additional_data[60];
//         let symmetry_fee =
//             SymmetryMath::mul_div(total_fees, symmetry_bps as u64, 100).context("mul div err")?;

//         let host_bps = token_list.list[0].additional_data[61];
//         let host_fee =
//             SymmetryMath::mul_div(total_fees, host_bps as u64, 100).context("mul div err")?;

//         let manager_bps = token_list.list[0].additional_data[62];
//         let manager_fee =
//             SymmetryMath::mul_div(total_fees, manager_bps as u64, 100).context("mul div err")?;

//         let fund_fee = total_fees - symmetry_fee - host_fee - manager_fee;

//         let fee_bps = SymmetryMath::mul_div(
//             amount_without_fees - to_amount,
//             BPS_DIVIDER * 100,
//             fair_amount,
//         )
//         .context("mul div err")?;

//         let from_token_worth_before_swap = SymmetryMath::amount_to_usd_value(
//             fund_state.current_comp_amount[from_token_index],
//             from_token_settings.decimals,
//             from_token_price.avg_price,
//         )
//         .context("mul div err")?;
//         let to_token_worth_before_swap = SymmetryMath::amount_to_usd_value(
//             fund_state.current_comp_amount[to_token_index],
//             to_token_settings.decimals,
//             to_token_price.avg_price,
//         )?;

//         let safe_from_amount = from_amount * 101 / 100;
//         let from_token_worth_after_swap = SymmetryMath::amount_to_usd_value(
//             fund_state.current_comp_amount[from_token_index] + safe_from_amount,
//             from_token_settings.decimals,
//             from_token_price.avg_price,
//         )?;
//         let mut safe_to_amount = (amount_without_fees - fund_fee) * 101 / 100;
//         if safe_to_amount > fund_state.current_comp_amount[to_token_index] {
//             safe_to_amount = fund_state.current_comp_amount[to_token_index];
//         }
//         let to_token_worth_after_swap = SymmetryMath::amount_to_usd_value(
//             fund_state.current_comp_amount[to_token_index] - safe_to_amount,
//             to_token_settings.decimals,
//             to_token_price.avg_price,
//         )?;

//         fund_worth = fund_worth + from_token_worth_after_swap;
//         fund_worth = fund_worth + to_token_worth_after_swap;
//         fund_worth = if fund_worth < from_token_worth_before_swap {
//             0
//         } else {
//             fund_worth - from_token_worth_before_swap
//         };
//         fund_worth = if fund_worth < to_token_worth_before_swap {
//             0
//         } else {
//             fund_worth - to_token_worth_before_swap
//         };

//         let from_new_weight =
//             SymmetryMath::mul_div(from_token_worth_after_swap, WEIGHT_MULTIPLIER, fund_worth)
//                 .context("mul div err")?;
//         let to_new_weight =
//             SymmetryMath::mul_div(to_token_worth_after_swap, WEIGHT_MULTIPLIER, fund_worth)
//                 .context("mul div err")?;

//         let allowed_offset = fund_state.rebalance_threshold * fund_state.lp_offset_threshold;

//         let mut allowed_from_target_weight = SymmetryMath::mul_div(
//             fund_state.target_weight[from_token_index],
//             BPS_DIVIDER * BPS_DIVIDER + allowed_offset,
//             BPS_DIVIDER * BPS_DIVIDER,
//         )
//         .context("mul div err")?;
//         let allowed_to_target_weight = SymmetryMath::mul_div(
//             fund_state.target_weight[to_token_index],
//             BPS_DIVIDER * BPS_DIVIDER - allowed_offset,
//             BPS_DIVIDER * BPS_DIVIDER,
//         )
//         .context("mul div err")?;
//         if allowed_from_target_weight > WEIGHT_MULTIPLIER {
//             allowed_from_target_weight = WEIGHT_MULTIPLIER;
//         }

//         let removing_dust = from_token_id == 0 && fund_state.target_weight[to_token_index] == 0;

//         if from_new_weight > allowed_from_target_weight && (!removing_dust) {
//             return Err(Error::msg("From token weight exceeds max allowed weight"));
//         }

//         if to_new_weight < allowed_to_target_weight {
//             return Err(Error::msg("To token weight exceeds min allowed weight"));
//         }

//         Ok(Quote {
//             in_amount: quote_params.amount,
//             out_amount: to_amount,
//             fee_amount: total_fees,
//             fee_mint: quote_params.output_mint,
//             fee_pct: Decimal::new(fee_bps as i64, 4),
//             ..Quote::default()
//         })
//     }

//     fn get_swap_and_account_metas(&self, swap_params: &SwapParams) -> Result<SwapAndAccountMetas> {
//         let SwapParams {
//             in_amount,
//             source_mint,
//             destination_mint,
//             source_token_account,
//             destination_token_account,
//             token_transfer_authority,
//             open_order_address,
//             quote_mint_to_referrer,
//             jupiter_program_id,
//             ..
//         } = swap_params;

//         let token_list = self
//             .token_list
//             .ok_or_else(|| anyhow!("token list is empty"))?;
//         let from_token_id_option = token_list
//             .list
//             .iter()
//             .position(|&x| x.token_mint == *source_mint);
//         let to_token_id_option = token_list
//             .list
//             .iter()
//             .position(|&x| x.token_mint == *destination_mint);

//         if from_token_id_option.is_none() {
//             return Err(Error::msg("From token not found in supported tokens"));
//         }
//         if to_token_id_option.is_none() {
//             return Err(Error::msg("To token not found in supported tokens"));
//         }

//         let from_token_id: u64 = from_token_id_option.unwrap() as u64;
//         let to_token_id: u64 = to_token_id_option.unwrap() as u64;

//         let swap_to_fee: Pubkey = Pubkey::find_program_address(
//             &[
//                 &SWAP_FEE_ADDRESS.to_bytes(),
//                 &spl_token::ID.to_bytes(),
//                 &destination_mint.to_bytes(),
//             ],
//             &spl_associated_token_account::ID,
//         )
//         .0;
//         let host_to_fee: Pubkey = Pubkey::find_program_address(
//             &[
//                 &self.fund_state.host_pubkey.to_bytes(),
//                 &spl_token::ID.to_bytes(),
//                 &destination_mint.to_bytes(),
//             ],
//             &spl_associated_token_account::ID,
//         )
//         .0;
//         let manager_to_fee: Pubkey = Pubkey::find_program_address(
//             &[
//                 &self.fund_state.manager.to_bytes(),
//                 &spl_token::ID.to_bytes(),
//                 &destination_mint.to_bytes(),
//             ],
//             &spl_associated_token_account::ID,
//         )
//         .0;

//         let mut account_metas: Vec<AccountMeta> = Vec::new();
//         account_metas.push(AccountMeta::new(*token_transfer_authority, true));
//         account_metas.push(AccountMeta::new(self.key, false));
//         account_metas.push(AccountMeta::new_readonly(PDA_ADDRESS, false));
//         account_metas.push(AccountMeta::new(
//             token_list.list[from_token_id as usize].pda_token_account,
//             false,
//         ));
//         account_metas.push(AccountMeta::new(*source_token_account, false));
//         account_metas.push(AccountMeta::new(
//             token_list.list[to_token_id as usize].pda_token_account,
//             false,
//         ));
//         account_metas.push(AccountMeta::new(*destination_token_account, false));
//         account_metas.push(AccountMeta::new(swap_to_fee, false));
//         account_metas.push(AccountMeta::new(host_to_fee, false));
//         account_metas.push(AccountMeta::new(manager_to_fee, false));
//         account_metas.push(AccountMeta::new_readonly(TOKEN_LIST_ADDRESS, false));
//         account_metas.push(AccountMeta::new_readonly(CURVE_DATA_ADDRESS, false));
//         account_metas.push(AccountMeta::new_readonly(spl_token::ID, false));

//         // Pyth Oracle accounts are being passed as remaining accounts
//         for i in 0..self.fund_state.num_of_tokens as usize {
//             account_metas.push(AccountMeta::new_readonly(
//                 token_list.list[self.fund_state.current_comp_token[i] as usize].oracle_account,
//                 false,
//             ));
//         }

//         let instruction_n: u64 = SYMMETRY_PROGRAM_SWAP_INSTRUCTION_ID;
//         let minimum_amount_out: u64 = 0;
//         let mut data = Vec::new();
//         data.extend_from_slice(&instruction_n.to_le_bytes());
//         data.extend_from_slice(&from_token_id.to_le_bytes());
//         data.extend_from_slice(&to_token_id.to_le_bytes());
//         data.extend_from_slice(&in_amount.to_le_bytes());
//         data.extend_from_slice(&minimum_amount_out.to_le_bytes());

//         let swap_instruction = Instruction {
//             program_id: SYMMETRY_PROGRAM_ADDRESS,
//             accounts: account_metas.clone(),
//             data,
//         };

//         Ok(SwapAndAccountMetas {
//             swap: Swap::TokenSwap,
//             account_metas,
//         })
//     }

//     fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
//         todo!("ignore")
//     }
// }

#[test]
fn test_symetry_token_swap() {
    const WSOL_TOKEN_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
    const USDC_TOKEN_MINT: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
    const USDT_TOKEN_MINT: Pubkey = pubkey!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB");
    const MSOL_TOKEN_MINT: Pubkey = pubkey!("mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So");

    use crate::amms::test_harness::AmmTestHarness;

    /* Init Token Swap */
    const TOKEN_LIST_ACCOUNT: Pubkey = SymmetryMath::TOKEN_LIST_ADDRESS;
    const FUND_STATE_ACCOUNT: Pubkey = pubkey!("2VHtUhF8KrjN4xx1fEsTB7Fcnw78DNHKwcjQF5ikFzqZ");

    let test_harness = AmmTestHarness::new();
    let fund_state_account = test_harness.get_keyed_account(FUND_STATE_ACCOUNT).unwrap();
    let token_list_account = test_harness.get_keyed_account(TOKEN_LIST_ACCOUNT).unwrap();
    let mut token_swap =
        SymmetryMath::from_keyed_account(&fund_state_account, &token_list_account).unwrap();

    /* Update TokenSwap (FundState + CurveData + Pyth Oracle accounts) */
    test_harness.update_amm(&mut token_swap);

    /* Token mints available for swap in a fund */
    println!("-------------------");
    let token_mints = token_swap.get_reserve_mints();
    println!("Available mints for swap: {:?}", token_mints);
    let from_token_mint: Pubkey = token_mints
        .clone()
        .into_iter()
        .find(|&x| x == WSOL_TOKEN_MINT)
        .unwrap();
    let to_token_mint: Pubkey = token_mints
        .clone()
        .into_iter()
        .find(|&x| x == MSOL_TOKEN_MINT)
        .unwrap();

    /* Get Quote */
    println!("-------------------");
    let in_amount: u64 = 100_000_000; // 0.1 WSOL -> ? MSOL
    let quote = token_swap
        .quote(&QuoteParams {
            input_mint: from_token_mint,
            in_amount: in_amount,
            output_mint: to_token_mint,
        })
        .unwrap();
    println!("Quote result: {:?}", quote);

    /* Get swap and account metas */
    println!("------------");
    let user = Pubkey::new_unique();
    let user_source = Pubkey::find_program_address(
        &[
            &user.to_bytes(),
            &SymmetryMath::spl_token::ID.to_bytes(),
            &from_token_mint.to_bytes(),
        ],
        &SymmetryMath::spl_associated_token_account::ID,
    )
    .0;
    let user_destination = Pubkey::find_program_address(
        &[
            &user.to_bytes(),
            &SymmetryMath::spl_token::ID.to_bytes(),
            &to_token_mint.to_bytes(),
        ],
        &SymmetryMath::spl_associated_token_account::ID,
    )
    .0;
    let swap_and_account_metas = token_swap
        .get_swap_and_account_metas(&SwapParams {
            in_amount: in_amount,
            source_mint: from_token_mint,
            destination_mint: to_token_mint,
            source_token_account: user_source,
            destination_token_account: user_destination,
            token_transfer_authority: user,
            open_order_address: Option::None,
            quote_mint_to_referrer: Option::None,
            jupiter_program_id: &Pubkey::default(),
        })
        .unwrap();
}
