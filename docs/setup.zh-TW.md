# 安裝與設定

[English](setup.md)

適用於 Linux 和 WSL2（Ubuntu 22.04 / 24.04）。本指南的步驟都不會在主網送出交易；實跑是最後另外一個明確的步驟，只能由你本人啟動。

## 1. 工具

```bash
sudo apt-get update
sudo apt-get install -y build-essential pkg-config git curl ca-certificates chrony
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y   # Rust（edition 2024：stable >= 1.85）
curl -L https://foundry.paradigm.xyz | bash && ~/.foundry/bin/foundryup  # forge、cast、anvil
```

## 2. 取得程式碼

```bash
git clone --recurse-submodules <repo-url> arbitrage-framework
cd arbitrage-framework
git submodule update --init --recursive    # 如果 clone 時沒加 --recurse-submodules（forge-std）
cd contracts && forge build && cd ..        # bot 報價時會注入編譯好的 GridQuoter
```

## 3. RPC 端點（`.env`）

```bash
cp .env.example .env    # 再填入你真正的 URL（這個檔案已被 gitignore）
```

RPC URL 含有 API key，所以只放在 `.env`。設定檔裡每個 `[[rpc]]` 用 `http_env` 指向變數名稱（公共端點可以直接用 `http`）；log 和錯誤訊息只會出現端點的 `name`。`[aero] rpc` 可以讓策略依嚴格順序只用自己的端點，避免和同一組 key 上的其他 bot 搶額度。同塊模式需要支援 `eth_simulateV1` 的端點。

## 4. 設定檔

```bash
cd bot
cp config.example.toml config.local.toml   # 已被 gitignore
```

檢查 `[[rpc]]`、`[exec]`（只放熱錢包的地址，絕不放私鑰）、`[flashblocks]` 和 `[aero]`：Aero／舊 Aerodrome 的地址（要在鏈上核對；協議遷移時地址會改變）、`loan_tokens`、利潤門檻、priority fee、`funding`，以及部署後的 `contract`。範例裡的數值只是讓 bot 能啟動的佔位值，不是建議：請依自己的風險決定規模和出價。

## 5. 編譯並觀察

```bash
cargo build --release
./target/release/arb-bot -c config.local.toml     # 只觀察：池子、價差、估算、精確檢查都寫進 journal
```

沒有設定 `[aero] contract`，或沒有加 `--live` 時，絕不會送出任何交易。進一步之前，先讀 journal（`[aero] journal_path`）。

## 6. 測試

```bash
cargo test                                      # 單元測試，包含用假策略跑的 runner 測試
cd ../contracts
forge test --no-match-contract Fork -vv         # 資金防護測試，不需 RPC
BASE_RPC_URL=<你的 url> forge test -j 1 -vv     # fork 測試；-j 1 避免免費方案的 RPC 被限流
```

fork 測試從 `bot/config.example.toml` 讀取 `[aero]` 的地址，沒有設定 `BASE_RPC_URL` 時預設用公共端點。含 key 的 URL 只能透過環境變數傳入。

要完整試跑部署腳本或 `--live`，請用本地 anvil fork、拋棄式的 keystore、緊急停止檔案，並在那份測試設定裡寫 `extra_broadcast = []`，確保什麼都不會送到主網。

## 7. 私鑰

```bash
cast wallet import bot-hot --interactive   # 熱錢包：只放 gas
```

私鑰只存在加密 keystore；絕不放進程式碼、設定檔、`.env`、shell 歷史或 log。bot 會從 `exec.keystore_password_env` 指定的環境變數讀取密碼（讀完立刻從 process 環境中刪除），或用提示輸入。利潤送到合約裡 immutable 的 `PROFIT_RECEIVER`（冷錢包），絕不會送到熱錢包。

## 8. 實跑（只能由 operator 執行）

1. 部署：`./scripts/deploy_aero_arb.sh <HOT> <COLD>`（從 `bot/config.local.toml [aero]` 讀取地址、在鏈上核對、以 `profitReceiver = COLD` 部署、把池子的 factory 加入白名單）。之後新增的 factory 可以用 `ARB=<地址>` 重跑來加入白名單。不再需要快速加白名單時，把 owner 交給 COLD：由 owner 呼叫 `transferOwnership(COLD)`，再執行 `./scripts/accept_ownership.sh <ARB> <COLD>`。這些腳本會簽主網交易，請自己執行。
2. 把地址填進 `[aero] contract`；選擇 `funding`（部署時沒有 V4 PoolManager 的合約只支援 `"morpho"`；bot 啟動時會檢查）。
3. 第一次實跑前，設定並再次確認：`[aero] priority_fee_wei` 和 `bid_max_priority_wei`（priority fee 上限）、`max_loan_usd`、利潤門檻、失敗冷卻（`max_failures_in_window` / `failure_window_secs` / `failure_cooldown_secs`），以及 `exec.kill_switch_file`。熱錢包的餘額要保持少量：它決定了失敗交易最多能燒掉多少。
4. 由你本人啟動 `./target/release/arb-bot -c config.local.toml --live`。合約的 executor、遷移合約、代幣、白名單或資金來源和設定不一致，或合約被暫停時，pre-flight 檢查會拒絕啟動。
5. 緊急停止：建立緊急停止檔案，或由 owner 或 executor 呼叫合約的 `pause()`（只有 owner 能解除）。

如果還有其他 bot 用同一個熱錢包簽名，兩邊都把 `exec.nonce_file` 留空，它們就會共用以錢包為單位的 nonce 鎖（`~/.local/state/onchain-bots/nonce-<錢包>`）。
