# 貢獻指南

[English](CONTRIBUTING.md)

感謝你的幫忙。有人實跑時，這個專案動用的是真金白銀，所以正確性和安全優先於新功能。

## 開始之前

- 先讀 [docs/architecture.zh-TW.md](docs/architecture.zh-TW.md)：策略驅動器（`OpportunitySource`）、
  兩種資金模型（`FlashLoan`、`FlashAccounting`），以及套利合約的安全模型。
- 較大的改動（新策略、新資金來源、改合約）請先開 issue 討論做法。
- 鏈的基礎管線（RPC 池、executor、關卡、模擬）放在 `onchain-core`，這是另一個 repo，和這個 repo
  並排 checkout；那裡的改動請在那個 repo 開 pull request。

## 開發環境

見 [docs/setup.zh-TW.md](docs/setup.zh-TW.md)。簡單說：Rust（stable）、Foundry、
`git submodule update --init`，fork 測試可另外指定 RPC 端點（預設用公共端點）。

## 每個 pull request 都要通過的檢查

```
cd bot && cargo check --all-targets     # 0 warnings
cd bot && cargo test
cd contracts && forge build && forge test -j 1   # fork 測試預設用公共端點，BASE_RPC_URL 可覆寫
```

## 原則

- **透過 trait 擴充。**新策略實作 `OpportunitySource`；新資金來源實作 `FlashLoan` 或
  `FlashAccounting`，並在合約加一個參數和現有入口相同的入口。
- **合約**：新的資金 callback 只能在由該來源出資的那筆套利裡執行，而且只能一次。請把這些情況加進
  `AeroMigrationArb.funding.t.sol`（錯誤的來源、第二次回呼、不回呼的資金方），並為真實的來源寫
  fork 測試。每條路徑都要保留最低利潤檢查和目標白名單。
- **hook 只有遇到致命錯誤才回傳 `Err`**；可恢復的錯誤記 log 後吞掉。
- 會遷移的 venue 地址**放設定檔，不要寫進程式碼**。
- **註解用英文**，風格跟周圍的程式一致。文件有英文和繁體中文（`*.zh-TW.md`）兩版：兩版都要更新，
  或在 PR 裡註明翻譯待補。
- commit 裡**不能有機密，也不能有私人策略資料**：不放私鑰、含 API key 的 RPC URL、個人錢包地址，
  以及你不想公開的調校參數。
- 測試和工具絕不送主網交易。

## 授權

提交貢獻即表示你同意你的貢獻和本專案其他部分一樣，採用
[GNU AGPL v3.0 或更新版本](LICENSE)授權。
