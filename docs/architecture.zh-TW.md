# 架構

[English](architecture.md)

這個框架用來開發原子性的鏈上套利 bot：偵測價差、規劃一組 swap、用暫時性資金（閃電貸或閃電記帳）支應、對合約做精確檢查，然後送出。可重複使用的基礎設施（區塊／flashblock 驅動、RPC 連線池、executor）是通用的；策略透過 `OpportunitySource` 接進來，資金來源透過 `FlashLoan` / `FlashAccounting` 接進來。**Base 上的 Aero 合併套利**是第一個完整的策略。

## 驅動：`runner::run` + `OpportunitySource`（`bot/src/runner.rs`）

```rust
pub trait OpportunitySource<P> {
    fn name(&self) -> &'static str;
    fn on_block(&mut self, p: &P, head: &Head, exec: Option<&mut Executor<P>>) -> impl Future<Output = Result<()>> + Send;
    fn on_flashblocks(&mut self, p: &P, first: Flashblock, rx: &mut mpsc::Receiver<Flashblock>,
                      exec: Option<&mut Executor<P>>, last_block: u64) -> impl Future<Output = Result<()>> + Send;
    fn on_outcome(&mut self, block: u64, inflight: &Inflight, outcome: &Outcome,
                  gas_spent_total_wei: U256, cooldown_started: bool) -> Result<()>;
}
```

`runner::run(&mut strategy, provider, executor, heads, flashblocks, RunnerOpts { poll_ms, ws_quiet })` 負責排程：

- 新區塊來自 WebSocket `newHeads`（多個端點），去重並保證嚴格遞增；WS 安靜超過 `ws_quiet` 時，每 `poll_ms` 用 HTTP 輪詢；
- 每個新區塊：先做 executor 的 receipt 輪詢（結果 -> `on_outcome`，含 gas 計算和失敗冷卻），再呼叫 `on_block`；
- 策略有同塊模式時，flashblocks 批次（第一個加上排在後面的）-> `on_flashblocks`。

hook 只在致命錯誤時回傳 `Err`（驅動會停止）；可恢復的錯誤由策略自己記 log 並吞掉。觀察模式下 `exec` 是 `None`。

## 資金：`FlashLoan` 和 `FlashAccounting`（`bot/src/flash.rs`）

刻意拆成兩個 trait，因為合約和它們的互動方式不同：

| Trait | 模型 | 方法 | 實作 |
|---|---|---|---|
| `FlashLoan` | 借固定金額；放款方回呼；在 callback 結束前還本金加手續費 | `name`、`lender`、`fee(amount)` | `MorphoBlue`（無手續費） |
| `FlashAccounting` | 單例合約在 unlock 期間追蹤呼叫者的 delta；先拿，在 `unlock` 結束前結清淨額 | `name`、`manager`、`fee(amount)`（預設 0） | `UniswapV4`（PoolManager） |

`flash::available(p, holder, tokens, block)` 讀取資金來源能借出多少（它的代幣餘額，一次 multicall）。資金本身是在套利合約裡取得的，由它的 callback 執行套利，所以鏈下的策略只需要知道哪個合約提供資金、成本多少、手上有多少。

## 鏈上：`contracts/src/AeroMigrationArb.sol`

套利本體（`_migrateArb`：買舊 AERO -> 遷移 -> 賣新 AERO）不知道資金從哪裡來。兩個**參數完全相同**的入口決定資金來源，所以兩者的 `minProfit` calldata 位置和回傳型別都一樣：

- `arb(loanToken, loanAmount, buy, sell, minProfit, deadlineBlock)`：Morpho Blue 閃電貸，`flashLoan` -> `onMorphoFlashLoan` -> 套利 -> 給還款剛好的授權。
- `arbFlashAccounting(...)`：Uniswap V4，`unlock` -> `unlockCallback` -> `take` -> 套利 -> `sync` / 轉帳 / `settle`，讓 delta 在 `unlock` 結束前歸零。constructor 傳入 `POOL_MANAGER = address(0)` 會停用它（`FundingDisabled`）。合約持有 V4 的 lock 期間，swap 步驟不能經過 V4 本身（V4 的 router 會再呼叫一次 `unlock`）。

swap 步驟（`ArbStep`）：kind 0 = 白名單內的 router，給剛好的授權（數量可以寫入 calldata）；kind 1/2 = 白名單 factory 的 Solidly 類池子（`isPool`），產出在鏈上用 `getAmountOut` / `getAmountOutWithTotalFee` 計算；kind 3 = CL 池，它的 factory 在白名單內，且 `getPool` 回傳的正是這個池子，在它自己的 swap callback 裡付款（只付給正在交易的池子、只付一次、最多 `amountIn`）。`amountIn = type(uint256).max` 表示用掉合約持有的全部 `tokenIn`。

### 安全模型

- 只有 `executor`（熱錢包）能發起套利；只有 `owner` 能改設定、解除暫停、移動資金；owner 轉移要兩步驟；executor 或 owner 可以 `pause`（給手上有熱錢包金鑰的操作者手動停用；bot 本身從不呼叫，它的緊急停止是一個檔案）。
- **transient 資金標記**：每個入口設定哪個來源可以回呼；callback 同時檢查呼叫者（資金方）和標記，然後清除標記。在 V4 資金的套利中來自 Morpho 的 callback（或反過來）、第二次 callback、不在自己套利中的 callback，都會 revert `Unauthorized`。
- 資金方沒有回呼就返回的入口會 revert `NotFunded`。
- **兩份白名單**：router（kind 0，會拿到授權和任意 calldata）放在 `allowedTarget`；池子 factory（kind 1～3，只會被問「這個池子是不是你的」）放在 `allowedFactory`（`setAllowedFactory`）。factory 永遠不能被當成 router 呼叫，router 也不能替池子背書。
- 授權都是剛好的數量，用完歸零；PoolManager 拿回的剛好是被借出的數量。
- 還款後剩下的借款代幣餘額低於 `minProfit` 就整筆 revert；利潤和剩下的 AERO 送到 immutable 的 `PROFIT_RECEIVER`；平常不留存任何資金。

### 測試

- `contracts/test/AeroMigrationArb.funding.t.sol`（不需 RPC）：一個惡意合約同時扮演 Morpho 和 PoolManager，所以保護套利的只有資金標記：跨來源的 callback、第二次 callback、不回呼的資金方都會被擋下；只回呼一次的誠實資金方會成功（對照組）。
- `contracts/test/AeroMigrationArb.fork.t.sol`（Base fork）：真實的遷移合約和舊池子，兩種資金路徑都測。V4 路徑剛好還清 PoolManager，利潤和 Morpho 路徑完全相同；在測試路線上用了 682k gas，Morpho 是 703k。另外測試最低利潤、白名單、假池子、CL callback 防護、executor／過期／暫停、停用 V4，以及 factory 和 router 是兩份獨立的白名單（`test_factoriesAndRoutersAreSeparateLists`）。固定在某個區塊上跑。
- `bot/src/aero.rs` 的 `e2e_arb_executor_sends_a_profitable_arb_on_anvil_fork`（ignored；本地 anvil fork）：像 owner 一樣部署合約並設定白名單，透過真實的舊池子和遷移合約（新 AERO 由固定匯率的替身 router 賣出）走 `exact` / `pick` / `terms` / `send` 和 executor（兩種資金來源都做精確檢查，送出較好的那個），並確認緊急停止檔案的關卡會擋下下一筆。
- `contracts/test/GridQuoter.fork.t.sol`：只用於模擬的報價合約。

## 送單流程：`ArbExecutor`（`bot/src/arb_exec.rs`）

從「策略找到機會」到「送出交易」之間的事情，適用於任何會回傳利潤的原子套利合約。策略依偏好順序，為每一種吃下機會的方式建一個 `Candidate { calldata, gas_est, floor_usd, l1_usd }`，並提供兩樣東西：怎麼呼叫（`call`：在它報價時的那個狀態上執行一個 `TransactionRequest` 的 closure，例如封裝好的區塊，或重播過的進行中區塊），以及怎麼把利潤代幣換算成 USD。

1. `exact(candidates, call)`：所有候選平行地對合約執行，計價方式和真實交易相同（基本 priority fee、gas limit、max fee = 2 × base fee + priority），因為有些 venue 會讀 `tx.gasprice`；回傳資料由合約專屬的 `decode` 解碼。
2. `pick(candidates, exact, usd)` -> `Picked { index, profit, gross_usd, net_usd }`：淨利 = 利潤減掉該候選的模型 gas（`gas_cost_usd`），再減掉它的 L1 data fee（`l1_usd`，由策略用 `l1_fee_wei` 填入，也就是 GasPriceOracle 依 calldata 大小給出的上限）；先選達到門檻的，再選淨利高的；完全平手時保留較前面的候選。
3. `terms(candidate, picked, balance, usd, call)` -> `SendTerms { priority_fee_wei, gas_limit, min_profit, bid_usd }`：`BidPolicy::priority` 可以拿大額利潤的一部分出價（不超過淨利減門檻，並受 `max_priority_wei` 限制）；出價會搭配較緊的 gas limit（模型的 2 倍），並在它自己的 priority 下重新檢查，在那裡過不了門檻就改回基本出價；錢包必須付得起 gas limit × max fee（否則回傳 None）；`min_profit` = 精確利潤 × `min_profit_keep`。
4. `request(candidate, terms, block, deadline)` -> executor 的 `SendRequest`（minProfit 會寫進合約的 `min_profit_slot`）。
5. `send(executor, candidate, terms, gates, block, deadline)`：先過所有策略共用的關卡，再交給 executor。`Gates { kill_switch_file, budget, daily_cap_usd, now_unix }`：緊急停止檔案、executor 的失敗冷卻，以及每日預算（候選交易萬一 revert 的最壞成本，也就是它 priority 下的 gas 加 L1 fee，必須放得進去）；任何一關擋下就回傳 `Ok(None)`，什麼都不送。

這個模組的 log 以 `arb: ...` 開頭，並帶有 `strategy` 欄位。它也提供每個策略送單時都需要的東西：`deadline_blocks(deadline_secs, block_time)`（把以秒計的期限依實測出塊時間換算成區塊數，出塊時間從 2 秒變 200 ms 也不必改設定），以及 `DailyBudget`（每個 UTC 日 revert 交易燒掉的 gas：revert 後 `charge`，送出前 `allows`）。

## 範例策略：Aero（`bot/src/aero.rs`）

舊 AERO 可以透過不需許可的 `Migration.depositLiquid` 以 1:1 換成新 AERO（只能單向）。只要舊 AERO 的價格低於新 AERO：借入借款代幣 -> 買舊 AERO（分散在舊 Aerodrome V2 和 Slipstream 池子）-> 遷移 -> 賣新 AERO（分散在 Aero V2 和 CL 池子）-> 還款，全部在一筆交易裡完成。

- **探索**（每幾十個區塊一次）：向每個設定的 factory，以及賣方的 Aero FactoryRegistry，查詢舊或新 AERO 對借款代幣的池子（CL 會查每一種 tick spacing）；沒有流動性的池子略過。地址放在 `[aero]`，不寫死在程式碼。
- **報價**（每個區塊，固定在 head 的區塊高度上，所以每次讀取看到的是同一個狀態）：一次 multicall 取得 USD 價格和 V2 報價；CL 池用 state override 注入的 `GridQuoter` 報價。買方曲線依一組借款金額報價，賣方曲線依一組 AERO 數量報價。
- **規劃**：每個借款金額把輸入 greedy 分配到各買方池（對凹曲線是最佳解），買到的舊 AERO 同樣分配到各賣方池；淨利最高的金額勝出。估算會扣掉已啟用來源中最便宜的閃電手續費和模型 gas。
- **精確檢查和送單**都透過 `ArbExecutor`：每個金額（最佳金額和它的鄰近金額，各自重新分配）× 每個啟用的資金來源各一個 `Candidate`，Morpho 排在前面，所以完全平手時選它；每個候選有自己的模型 gas（`arb_gas`）和依借款金額的門檻。`call` 在封裝好的區塊上執行，同塊模式下則在重播過的進行中區塊上執行。L1 data fee（和精確檢查一起讀一次，以最大的 calldata 為準）計入每個候選的淨利。只有精確檢查的結果會被送出，`minProfit = 精確利潤 × min_profit_keep`，期限是 `[aero] deadline_secs` 依區塊頭實測的出塊時間換算。
- **每日 gas 預算**：一筆交易若 revert 時的最壞成本（以它的 priority 計的 L2 gas + L1 fee）會讓今天 revert 套利的花費超過 `[exec] daily_gas_budget_usd`，就不送；`on_outcome` 會把每筆 revert 套利的實際成本記上。
- **USD 價格**：不是 USD 穩定幣的借款代幣會在同一個 multicall 裡計價：有設定 Chainlink feed（`usd_feed`，8 位小數，`latestRoundData`）而且夠新（在 `[aero] usd_feed_max_age_secs` 內更新過，預設 3600）就用 feed，否則用池子報價（`usd_pool`，`getAmountOut`）；優先用 feed，因為池子可能被抽乾或被推動。範例用 Base 的 ETH/USD feed 為 WETH 計價。第一個有計價的代幣也提供 `eth_usd`。
- **資金**（`[aero] funding`）：`"morpho"`（預設）呼叫 `arb`；`"v4"` 呼叫 `arbFlashAccounting`；`"auto"` 每個金額都同時用兩個入口精確檢查（eth_call 變兩倍），依各來源自己的模型 gas 扣除後送出淨利較高的。只要設定了 `[aero] contract`（觀察模式也一樣），bot 就會檢查合約的 `MORPHO()` / `POOL_MANAGER()` 和設定一致，不一致就拒絕啟動。實跑的 pre-flight 也會拒絕 `PROFIT_RECEIVER` 是熱錢包的合約。
- **實跑**需要設定檔裡的 `[exec] mode = "live"` 和命令列的 `--live` 兩者同時成立（單靠一個誤加的旗標或一份複製來的設定都不會送單），另外還要 `exec.keystore` 和 `[aero] contract`。
- **同塊模式**（`[aero] same_block`，Base Flashblocks）：只要進行中區塊的 flashblocks 帶有可能影響我們池子的交易，就在該區塊上跑同一套流程；那些交易會先在封裝好的區塊上重播，再做報價和精確檢查，所以區塊前段打開的價差可以在同一塊被吃掉。learner 會記錄哪些交易目標真的動到我們的池子，無關的交易不會觸發流程。
- **priority fee**：`bid_policy` 把 `[aero]`（`priority_fee_wei`、`bid_min_profit_usd`、`bid_share`、`bid_max_priority_wei`）轉成 `BidPolicy`，由 `ArbExecutor::terms` 套用：基本的 priority fee，大額精確利潤時可以拿出一部分出價；出價會在它自己的 priority 下重新檢查（Aero 池子會對 priority fee 抽稅），並受錢包餘額限制。
- **journal**：寫在 `[aero] journal_path` 的 JSON 行（`aero_start`、`aero_venue`、`aero_tick`、`aero_opp`、`aero_sent`、`aero_outcome`、`aero_cooldown`、`aero_learned`、`aero_same_block`）；沒有設定 `contract` 或沒有 `--live` 時只觀察。

## 基礎設施

| 模組 | 職責 |
|---|---|
| `config` | 套利專用設定和 RPC 解析（URL 只從 `.env` 來）；相對路徑的 `kill_switch_file` 以設定檔所在目錄為基準 |
| `chain` | 與鏈無關的讀取（Multicall3） |
| `num` | 單位換算工具 |
| `rpc` | 多端點連線池：競速／備援、每個端點的方法和 log 範圍限制、落後處理；容量錯誤以片語判斷（限流／方案／範圍限制），確定性錯誤（revert、gas limit、餘額不足、nonce……）絕不換端點；log 只顯示端點名稱 |
| `heads` | 新區塊訊號（WS `newHeads`，多個端點） |
| `flashblocks` | Base Flashblocks 原始串流 |
| `executor` | keystore 簽名、一次一筆交易在飛、透過暖機中的連線廣播給所有端點（加上可選的 `extra_broadcast`）、以錢包為單位共用的 nonce 鎖（`~/.local/state/onchain-bots/nonce-<錢包>`；fail closed：鎖不到就不送）、失敗冷卻（`[exec]` 的 `failure_window_secs` 內 `max_failures_in_window` 筆失敗就暫停送單 `failure_cooldown_secs`） |
| `runner` | 上面說的驅動 |
| `arb_exec` | `ArbExecutor`：精確檢查、挑選、出價、送單條件（見上方） |
| `flash` | 資金來源 |
| `aero` | 範例策略 |

## 加入一個策略

1. 建立 `bot/src/<strategy>.rs`，用一個 struct 保存策略狀態，constructor 負責驗證它的設定區段並做一次性的讀取。
2. 實作 `OpportunitySource`：`on_block` 處理每個封裝好的區塊；有同塊模式才需要 `on_flashblocks`（否則忽略那批資料）；`on_outcome` 處理你送出的交易的結果。
3. 只在致命錯誤時回傳 `Err`。可恢復的錯誤（RPC 不穩、報價失敗）記 log 即可。
4. 在 `main.rs` 呼叫 `runner::run(&mut strategy, provider, executor, heads, flashblocks, opts)`。
5. `runner.rs` 裡的 runner 測試用假的策略驅動 driver；照著它為你的策略寫一個。
6. 透過 `ArbExecutor` 送單（參考 Aero 的 `tick`）：建好 `Candidate`，提供你的 `call` 和 USD 換算，然後 `exact` -> `pick` -> `terms` -> `request` -> `Executor::send`。你的合約必須回傳利潤（設定 `decode`），並在 calldata 裡接收 `minProfit`（`min_profit_slot`），這樣精確檢查才準確，合約也會強制執行門檻。

## 加入一個資金來源

1. 在 `flash.rs` 實作 `FlashLoan`（回呼並收手續費）或 `FlashAccounting`（先拿後結清）。
2. 在合約加一個和現有入口**參數相同**的入口、一個資金標記常數，以及一個 callback：檢查呼叫者和標記、清除標記、執行 `_migrateArb`（或你的本體）、還款。保留 `_finish` 裡的 `NotFunded` 和 `minProfit` 檢查。
3. 擴充策略的資金來源清單（`aero.rs` 裡的 `Funder`）：名稱、持有資金的合約、手續費、gas 差額、calldata 編碼。
4. 測試：把新來源加進惡意資金方測試（`AeroMigrationArb.funding.t.sol`），並寫一個 fork 測試，證明它剛好還清，而且利潤和其他來源一致。

## 常用指令

```bash
cd contracts && forge build
forge test --no-match-contract Fork -vv          # 資金防護測試，不需 RPC
BASE_RPC_URL=<url> forge test -j 1 -vv           # fork 測試（預設用公共端點）
cd ../bot && cargo test
./target/release/arb-bot -c config.local.toml            # 只觀察
./target/release/arb-bot -c config.local.toml --live     # 實跑：只能由 operator 啟動
```
