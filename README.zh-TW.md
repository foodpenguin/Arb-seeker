# Arb-seeker

[English](README.md)

用 Rust（[alloy](https://github.com/alloy-rs/alloy)）和 Solidity（[Foundry](https://getfoundry.sh)）
開發原子性鏈上**套利 bot** 的框架。區塊／flashblock 迴圈、RPC 池、送單，以及暫時性資金的選擇都是
通用的；策略透過 trait 接上，而任何能寫成一串步驟的交易，都能在同一個通用合約（`StepArb.sol`）上
執行。範例策略是 **Base 上的 Aero 合併套利**（舊 AERO 可以 1:1 換成新 AERO：在同一筆交易裡買舊的、
遷移、賣新的、還款，不需要自有資金）。鏈的基礎管線來自
[bot-onchain-core](https://github.com/foodpenguin/bot-onchain-core)（crate `onchain-core`），和
[Debt-keeper](https://github.com/foodpenguin/Debt-keeper) 共用。這個 crate 名稱是 `arbitrage-framework`。

> **狀態：**實驗性。這裡沒有任何投資建議，也沒有替你調好的參數：範例設定用的是中性值。請先用
> 觀察模式跑、在 anvil fork 上測試送單，並有學習期間會虧 gas 的心理準備。

## 功能

- **通用策略驅動器**（`runner::run` + `OpportunitySource`）：用 WebSocket 收新區塊，WS 沒動靜時改用
  HTTP 輪詢；Base flashblocks 批次供同區塊策略使用；executor 作為獨立的 task（receipt 在背景輪詢，
  已結算的交易交給策略）；gas 記帳。
- **通用送單流程**（`arb_exec::ArbExecutor`）：所有候選平行地對合約精確檢查、挑選（先看門檻，再看
  淨利）、可選的 priority fee 出價（在它自己的 priority 下重新檢查）、熱錢包付得起的送單條件，以及
  每次送單都要通過的關卡（緊急停止開關、失敗冷卻、revert 交易 gas 的每日預算）。策略只要提供候選、
  怎麼呼叫，以及利潤代幣怎麼換算成 USD。
- **`StepArb.sol`**，通用的閃電出資套利合約：借款、執行一串步驟（白名單內的 router／target 呼叫，
  給剛好的授權；Solidly 和 CL 池會向白名單內的 factory 驗證）、還款、至少保留 minProfit、把列出的
  中間代幣清走。`step_arb.rs` 負責編碼。新策略只要能把交易寫成步驟，就不需要自己的合約。
- **兩種資金模型**（`flash.rs`），刻意分成兩個 trait：
  - `FlashLoan`：借固定數量，在 callback 裡還本金加手續費（`MorphoBlue`，0 手續費）；
  - `FlashAccounting`：先拿，在 `unlock` 結束前結清淨額（`UniswapV4` PoolManager）。
- **`FlashArbBase.sol`**，這裡每個套利合約共用的部分：參數完全相同的 `arb()`（Morpho）和
  `arbFlashAccounting()`（V4）兩個入口；每個 callback 只在由該來源出資的那筆套利裡執行、只能一次；
  只允許 executor 呼叫；owner 的兩份白名單：target（`allowedTarget`）和池子 factory
  （`allowedFactory`，池子會向它驗證）；最低利潤檢查。`AeroMigrationArb.sol` 是建立在它之上的 Aero
  本體（內建遷移）；同一個套利也能在 `StepArb` 上執行（`[aero] contract_kind = "steps"`）。
  `[aero] funding = "morpho" | "v4" | "auto"`。
- **Aero 範例**：從 factory 和 registry 找池子、用 state override 批次網格報價（`GridQuoter`）、
  在 V2 和 CL 池之間貪婪拆單、送出前精算實際交易，以及可選的同區塊模式（重播進行中區塊裡相關的
  交易）。
- **執行管線**（`onchain-core`）：多端點 RPC 池、共用同一個熱錢包的 bot 之間共用的版本化 nonce 檔、
  失敗冷卻、預熱的廣播連線、緊急停止開關。

## 目錄

```
bot/src/
  lib.rs         函式庫；main.rs 是建立在它之上的 Aero bot
  runner.rs      OpportunitySource + 通用驅動器
  arb_exec.rs    ArbExecutor：精確檢查 -> 挑選 -> 出價 -> 送單條件 -> 關卡
  step_arb.rs    通用的 StepArb 合約：calldata、minProfit 位置、利潤解碼
  contracts.rs   套利合約的 ABI
  flash.rs       FlashLoan / FlashAccounting（MorphoBlue、UniswapV4）
  aero.rs        Aero 合併套利（範例策略）
contracts/src/
  FlashArbBase.sol       資金、步驟、安全模型、管理功能（共用）
  StepArb.sol            通用套利：任何交易都寫成步驟
  AeroMigrationArb.sol   Aero 套利（內建遷移）
  sim/GridQuoter.sol     用 state override 注入的報價合約（只用於模擬）
scripts/
  deploy_aero_arb.sh     部署（KIND=steps：StepArb）+ 白名單（由操作者執行）
  accept_ownership.sh    owner 錢包接受兩階段的所有權轉移
docs/            架構與安裝（English / 繁體中文）
```

## 編譯

`onchain-core` 是鎖定 tag 的 git 依賴（`bot/Cargo.toml`），cargo 會自己抓，不必手動 checkout。要改用
本機 checkout 的 bot-onchain-core 編譯（例如兩邊同時在改），在 `bot/` 目錄覆寫依賴：

```
cargo test --config 'patch."https://github.com/foodpenguin/bot-onchain-core".onchain-core.path="../../bot-onchain-core"'
```

覆寫會改動 `bot/Cargo.lock`，這個變更不要 commit。

## 快速開始

完整步驟見 [docs/setup.zh-TW.md](docs/setup.zh-TW.md)。

```
cp .env.example .env                                  # 你的 RPC URL（不進 git）
cd contracts && forge build && cd ..
cd bot && cp config.example.toml config.local.toml    # 調整上限；[aero] contract 先不設
cargo build --release
./target/release/arb-bot -c config.local.toml         # 只觀察：不送任何交易
```

`--live` 還需要設定檔裡的 `[exec] mode = "live"`、`exec.keystore` 和已部署的 `[aero] contract`，而且一定由你自己啟動。revert 的套利每個 UTC 日最多燒掉 `[exec] daily_gas_budget_usd` 的 gas。

## 擴充

- **其他策略**：實作 `runner::OpportunitySource`（`on_block`、`on_flashblocks`、`on_outcome`），
  再呼叫 `runner::run`。hook 只有遇到致命錯誤才回傳 `Err`。把交易寫成給 `StepArb` 用的 `ArbStep`
  （`step_arb::calldata`），送單和 Aero 一樣透過 `arb_exec::ArbExecutor`（建好 `Candidate`，提供
  `call` 和 USD 換算）。
- **其他資金來源**：實作 `FlashLoan` 或 `FlashAccounting`，並在合約加一個參數和現有入口相同的入口，
  以及只認該來源的 callback。

完整教學在 [docs/architecture.zh-TW.md](docs/architecture.zh-TW.md)。

## 測試

```
cd bot && cargo check --all-targets && cargo test
cd bot && cargo test e2e -- --ignored                   # 在本地 anvil fork 上測試送單路徑，兩種合約都測
cd contracts && forge test --no-match-contract Fork     # 安全測試，不需要 RPC
cd contracts && forge test -j 1                         # 加上 fork 測試（預設用公共端點；BASE_RPC_URL 可覆寫）
```

## 安全

- 私鑰只放在加密 keystore（`cast wallet import`），絕不寫進程式碼、設定檔、`.env` 或 log。熱錢包
  只放 gas；利潤轉到合約裡不可變的收款地址。
- RPC URL（含 API key）放在 `.env`；log 只顯示端點名稱。
- 實跑和部署一定由操作者自己啟動。任何會送交易的東西，先在 anvil fork 上用
  `extra_broadcast = []` 和緊急停止檔測試。
- 多個 bot 可以用同一個熱錢包簽名；它們透過每個錢包一個鎖檔協調 nonce（預設
  `~/.local/state/onchain-bots/nonce-<wallet>`；`exec.nonce_file` 請留空）。它們都必須建立在同一個
  `onchain-core` nonce 檔版本上。

發現漏洞？請見 [SECURITY.md](SECURITY.md)。

## 貢獻與授權

請見 [CONTRIBUTING.zh-TW.md](CONTRIBUTING.zh-TW.md)。採用
[GNU Affero General Public License v3.0 或更新版本](LICENSE)授權。

本軟體不提供任何擔保。在主網上使用可能造成資金損失。
