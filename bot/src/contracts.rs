//! The arb contracts' ABIs, in one `sol!` block (custom types must share it): `ArbStep`, the
//! generic `StepArb`, and the example strategy's `AeroMigrationArb`.

use alloy::sol;

sol! {
    /// One step of an arb contract (FlashArbBase.sol `ArbStep`): kind 0 = allowlisted target with
    /// an exact allowance (amount patched at `patchOffset` when non-zero), 1 / 2 = Solidly pool,
    /// 3 = CL pool; `amountIn` = U256::MAX spends the contract's whole balance of `tokenIn`.
    #[derive(Debug, PartialEq)]
    struct ArbStep { uint256 kind; address target; address tokenIn; uint256 amountIn; uint256 patchOffset; bytes data; }

    /// The generic step-execution arb (StepArb.sol).
    #[sol(rpc)]
    interface IStepArb {
        function arb(address loanToken, uint256 loanAmount, ArbStep[] steps, address[] sweep, uint256 minProfit, uint256 deadlineBlock) external returns (uint256);
        function arbFlashAccounting(address loanToken, uint256 loanAmount, ArbStep[] steps, address[] sweep, uint256 minProfit, uint256 deadlineBlock) external returns (uint256);
    }

    /// The Aero merger arb (AeroMigrationArb.sol), plus the admin views both contracts share.
    #[sol(rpc)]
    interface IAeroArb {
        function arb(address loanToken, uint256 loanAmount, ArbStep[] buy, ArbStep[] sell, uint256 minProfit, uint256 deadlineBlock) external returns (uint256);
        function arbFlashAccounting(address loanToken, uint256 loanAmount, ArbStep[] buy, ArbStep[] sell, uint256 minProfit, uint256 deadlineBlock) external returns (uint256);
        function MORPHO() external view returns (address);
        function POOL_MANAGER() external view returns (address);
        function executor() external view returns (address);
        function PROFIT_RECEIVER() external view returns (address);
        function paused() external view returns (bool);
        function allowedTarget(address) external view returns (bool);
        function allowedFactory(address) external view returns (bool);
        function MIGRATION() external view returns (address);
        function NEW_TOKEN() external view returns (address);
    }

    interface IAeroMigration {
        /// Old AERO -> new AERO, 1:1, sent to `recipient`.
        function depositLiquid(uint256 amount, address recipient) external payable;
    }
}
