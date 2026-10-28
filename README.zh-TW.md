# arbitrage-framework

[English](README.md)

用 Rust（[alloy](https://github.com/alloy-rs/alloy)）和 Solidity（[Foundry](https://getfoundry.sh)）
開發原子性鏈上**套利 bot** 的框架。區塊／flashblock 迴圈、RPC 池、送單，以及暫時性資金的選擇都是
通用的；策略透過 trait 接上。範例策略是 **Base 上的 Aero 合併套利**（舊 AERO 可以 1:1 換成新
AERO：在同一筆交易裡買舊的、遷移、賣新的、還款，不需要自有資金）。

> **狀態：**實驗性。這裡沒有任何投資建議，也沒有替你調好的參數：範例設定用的是中性值。請先用
> 觀察模式跑、在 anvil fork 上測試送單，並有學習期間會虧 gas 的心理準備。

## 功能

- **通用策略驅動器**（`runner::run` + `OpportunitySource`）：用 WebSocket 收新區塊，WS 沒動靜時改用
  HTTP 輪詢；Base flashblocks 批次供同區塊策略使用；每個區塊輪詢進行中交易的收據；gas 記帳。
- **通用送單流程**（`arb_exec::ArbExecutor`）：所有候選平行地對合約精確檢查、挑選（先看門檻，再看淨利）、可選的 priority fee 出價（在它自己的 priority 下重新檢查）、熱錢包付得起的送單條件。策略只要提供候選、怎麼呼叫，以及利潤代幣怎麼換算成 USD。
- **兩種資金模型**（`flash.rs`），刻意分成兩個 trait：
  - `FlashLoan`：借固定數量，在 callback 裡還本金加手續費（`MorphoBlue`，0 手續費）；
  - `FlashAccounting`：先拿，在 `unlock` 結束前結清淨額（`UniswapV4` PoolManager）。
- **`AeroMigrationArb.sol`**：套利本體不知道資金來源；`arb()`（Morpho）和
  `arbFlashAccounting()`（V4）參數完全相同。每個 callback 只在由該來源出資的那筆套利裡執行、只能
  一次；只允許 executor 呼叫；owner 的兩份白名單：router（`allowedTarget`）和池子 factory（`allowedFactory`，池子會向它驗證）；最低利潤
  檢查。`[aero] funding = "morpho" | "v4" | "auto"`。
- **Aero 範例**：從 factory 和 registry 找池子、用 state override 批次網格報價（`GridQuoter`）、
  在 V2 和 CL 池之間貪婪拆單、送出前精算實際交易，以及可選的同區塊模式（重播進行中區塊裡相關的
  交易）。
- **執行管線**：多端點 RPC 池、共用熱錢包的多個 bot 之間的 nonce 協調、失敗冷卻、預熱的廣播連線、
  緊急停止開關。

## 目錄

```
bot/src/
  runner.rs      OpportunitySource + 通用驅動器
  arb_exec.rs    ArbExecutor：精確檢查 -> 挑選 -> 出價 -> 送單條件
  flash.rs       FlashLoan / FlashAccounting（MorphoBlue、UniswapV4）
  aero.rs        Aero 合併套利（範例策略）
  executor.rs    簽名、廣播、nonce 檔、冷卻
  rpc.rs heads.rs flashblocks.rs chain.rs num.rs   鏈的基礎管線
contracts/src/
  AeroMigrationArb.sol   套利合約（每個資金來源一個入口）
  sim/GridQuoter.sol     用 state override 注入的報價合約（只用於模擬）
scripts/
  deploy_aero_arb.sh     部署並把池子 factory 加入白名單（由操作者執行）
  accept_ownership.sh    owner 錢包接受兩階段的所有權轉移
docs/            架構與安裝（English / 繁體中文）
```

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
  再呼叫 `runner::run`。hook 只有遇到致命錯誤才回傳 `Err`。送單和 Aero 一樣透過
  `arb_exec::ArbExecutor`（建好 `Candidate`，提供 `call` 和 USD 換算）。
- **其他資金來源**：實作 `FlashLoan` 或 `FlashAccounting`，並在合約加一個參數和現有入口相同的入口，
  以及只認該來源的 callback。

完整教學在 [docs/architecture.zh-TW.md](docs/architecture.zh-TW.md)。

## 測試

```
cd bot && cargo check --all-targets && cargo test
cd contracts && forge test --no-match-contract Fork     # 資金防護測試，不需要 RPC
cd contracts && forge test -j 1                         # 加上 fork 測試（預設用公共端點；BASE_RPC_URL 可覆寫）
```

## 安全

- 私鑰只放在加密 keystore（`cast wallet import`），絕不寫進程式碼、設定檔、`.env` 或 log。熱錢包
  只放 gas；利潤轉到合約裡不可變的收款地址。
- RPC URL（含 API key）放在 `.env`；log 只顯示端點名稱。
- 實跑和部署一定由操作者自己啟動。任何會送交易的東西，先在 anvil fork 上用
  `extra_broadcast = []` 和緊急停止檔測試。
- 多個 bot 可以用同一個熱錢包簽名；它們透過每個錢包一個鎖檔協調 nonce（預設
  `~/.local/state/onchain-bots/nonce-<wallet>`；`exec.nonce_file` 請留空）。

發現漏洞？請見 [SECURITY.md](SECURITY.md)。

## 貢獻與授權

請見 [CONTRIBUTING.zh-TW.md](CONTRIBUTING.zh-TW.md)。採用
[GNU Affero General Public License v3.0 或更新版本](LICENSE)授權。

本軟體不提供任何擔保。在主網上使用可能造成資金損失。
