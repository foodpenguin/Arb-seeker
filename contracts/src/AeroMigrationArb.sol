// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {IV3Pool, MIN_SQRT_PRICE_LIMIT, MAX_SQRT_PRICE_LIMIT} from "./interfaces/IV3Pool.sol";

interface IERC20Bal {
    function balanceOf(address) external view returns (uint256);
}

/// @dev Flash loan (fixed amount, repaid inside the callback): Morpho Blue, 0 fee.
interface IMorphoFlash {
    function flashLoan(address token, uint256 assets, bytes calldata data) external;
}

/// @dev Flash accounting (take now, settle the net delta before unlock returns): Uniswap V4
///      PoolManager. `Currency` is an address in the ABI.
interface IPoolManager {
    function unlock(bytes calldata data) external returns (bytes memory);
    function take(address currency, address to, uint256 amount) external;
    function sync(address currency) external;
    function settle() external payable returns (uint256 paid);
}

interface IAeroMigration {
    /// @notice Old AERO -> new AERO, 1:1, sent to `recipient` (Aero merger, Base).
    function depositLiquid(uint256 amount, address recipient) external payable;
}

interface ISolidlyPool {
    function factory() external view returns (address);
    function token0() external view returns (address);
    function getAmountOut(uint256 amountIn, address tokenIn) external view returns (uint256);
    function getAmountOutWithTotalFee(uint256 amountIn, address tokenIn) external view returns (uint256);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
}

interface ISolidlyFactory {
    function isPool(address pool) external view returns (bool);
}

/// @dev Aerodrome Slipstream / Aero CL pool and factory surface used by kind 3.
interface IClPool {
    function factory() external view returns (address);
    function tickSpacing() external view returns (int24);
}

interface IClFactory {
    function getPool(address tokenA, address tokenB, int24 tickSpacing) external view returns (address);
}

/// @notice One swap step; `amountIn` = type(uint256).max spends this contract's whole balance of
///         `tokenIn` (a balance only known on-chain, e.g. what the migration paid out).
///         kind 0 (router): `target` gets an exact allowance and is called with `data`; when
///           `patchOffset` is non-zero, the amount is also written into `data` at that byte offset
///           (the router's amountIn field).
///         kind 1 / 2 (Solidly-style pool, direct): `target` is the pool. It must belong to an
///           allowlisted factory (`isPool`); the input is sent to it and the output computed with
///           `getAmountOut` (1: legacy Aerodrome) or `getAmountOutWithTotalFee` (2: Aero pools,
///           dynamic + MEV fee), in the same tx, so no quote can go stale. `data` is unused.
///         kind 3 (CL pool, direct: legacy Slipstream or Aero CL): `target` is the pool. Its
///           factory must be allowlisted and return this exact pool from getPool(token0, token1,
///           tickSpacing); the pool is paid inside its own swap callback, at most `amountIn`.
struct ArbStep {
    uint256 kind;
    address target;
    address tokenIn;
    uint256 amountIn;
    uint256 patchOffset;
    bytes data;
}

/// @title AeroMigrationArb
/// @notice Old AERO converts 1:1 into new AERO (Migration.depositLiquid, permissionless). When old
///         AERO trades below new AERO, this buys old with flash-borrowed capital, migrates, sells
///         new and repays, all in one tx, without capital.
/// @dev The arb body (`_migrateArb`) does not know where the capital came from. Two entry points
///      with the same arguments (so the same minProfit calldata offset) pick the funding:
///      - `arb`: Morpho Blue flash loan (0 fee, deepest on Base);
///      - `arbFlashAccounting`: Uniswap V4 PoolManager take + settle (0 fee). Swap steps must not
///        go through V4 then (the manager is already unlocked by us; a V4 router would revert).
///      Same security model as MorphoLiquidator:
///      - only `executor` (hot wallet) can start; only `owner` (cold wallet) changes settings,
///        unpauses or moves funds; two-step ownership;
///      - each funding callback only runs inside our own arb funded by that very source, once
///        (transient funding tag + the funder as sender); an entry whose callback never ran reverts;
///      - swap steps may only call owner-allowlisted targets, with exact allowances reset to zero;
///      - Morpho gets an exact allowance for the repayment; the PoolManager is paid exactly what we
///        took;
///      - the whole tx reverts unless the loan-token balance left after repayment is at least
///        `minProfit`; profit and any leftover old/new AERO go to PROFIT_RECEIVER (the contract
///        holds nothing at rest).
contract AeroMigrationArb {
    uint256 private constant FUND_MORPHO = 1;
    uint256 private constant FUND_V4 = 2;

    IMorphoFlash public immutable MORPHO;
    /// @dev address(0): flash accounting disabled (`arbFlashAccounting` reverts).
    IPoolManager public immutable POOL_MANAGER;
    IAeroMigration public immutable MIGRATION;
    address public immutable OLD_TOKEN;
    address public immutable NEW_TOKEN;
    address public immutable PROFIT_RECEIVER;

    address public owner;
    address public executor;
    bool public paused;
    address public pendingOwner;
    mapping(address => bool) public allowedTarget;

    /// @dev Which funder may call back right now (FUND_*); 0 = none / already called back.
    uint256 private transient fundingInFlight;
    address private transient loanInFlight;
    uint256 private transient migratedInFlight;
    /// @dev CL pool whose swap() is running right now, what it may be paid in, and at most how much.
    address private transient activePool;
    address private transient activeToken;
    uint256 private transient activeMaxIn;

    error Unauthorized();
    error Expired();
    error IsPaused();
    error TargetNotAllowed(address target);
    error InsufficientProfit(uint256 profit, uint256 minProfit);
    error SwapFailed(uint256 leg, uint256 index, bytes reason);
    error TokenCallFailed(address token);
    error NothingBought();
    error BadPatch();
    error BadKind();
    error FundingDisabled();
    error NotFunded();

    event ExecutorSet(address executor);
    event OwnershipTransferStarted(address indexed pendingOwner);
    event OwnerSet(address owner);
    event PausedSet(bool paused);
    event TargetAllowed(address indexed target, bool allowed);
    event Arb(address indexed loanToken, uint256 loanAmount, uint256 migrated, uint256 profit);

    modifier onlyOwner() {
        if (msg.sender != owner) revert Unauthorized();
        _;
    }

    constructor(
        IMorphoFlash morpho,
        IPoolManager poolManager,
        IAeroMigration migration,
        address oldToken,
        address newToken,
        address owner_,
        address executor_,
        address profitReceiver_
    ) {
        MORPHO = morpho;
        POOL_MANAGER = poolManager;
        MIGRATION = migration;
        OLD_TOKEN = oldToken;
        NEW_TOKEN = newToken;
        PROFIT_RECEIVER = profitReceiver_;
        owner = owner_;
        executor = executor_;
        emit OwnerSet(owner_);
        emit ExecutorSet(executor_);
    }

    /// @notice Funded by a Morpho Blue flash loan.
    /// @param loanToken Flash-borrowed (e.g. USDC); profit is counted in it.
    /// @param buy Steps turning `loanAmount` of loanToken into old AERO.
    /// @param sell Steps turning the migrated new AERO back into loanToken.
    /// @param minProfit Minimum loanToken left after repaying the flash loan; reverts below it.
    /// @param deadlineBlock Reverts if included after this block (stale opportunity).
    function arb(
        address loanToken,
        uint256 loanAmount,
        ArbStep[] calldata buy,
        ArbStep[] calldata sell,
        uint256 minProfit,
        uint256 deadlineBlock
    ) external returns (uint256 profit) {
        _begin(loanToken, deadlineBlock, FUND_MORPHO);
        MORPHO.flashLoan(loanToken, loanAmount, abi.encode(buy, sell));
        profit = _finish(loanToken, loanAmount, minProfit);
    }

    /// @notice Same as `arb`, funded by Uniswap V4 flash accounting (take from the PoolManager,
    ///         settle before unlock returns).
    function arbFlashAccounting(
        address loanToken,
        uint256 loanAmount,
        ArbStep[] calldata buy,
        ArbStep[] calldata sell,
        uint256 minProfit,
        uint256 deadlineBlock
    ) external returns (uint256 profit) {
        if (address(POOL_MANAGER) == address(0)) revert FundingDisabled();
        _begin(loanToken, deadlineBlock, FUND_V4);
        POOL_MANAGER.unlock(abi.encode(loanAmount, buy, sell));
        profit = _finish(loanToken, loanAmount, minProfit);
    }

    /// @dev Morpho sent `assets` of the loan token; it pulls them back right after this returns.
    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external {
        if (msg.sender != address(MORPHO) || fundingInFlight != FUND_MORPHO) revert Unauthorized();
        fundingInFlight = 0;
        (ArbStep[] memory buy, ArbStep[] memory sell) = abi.decode(data, (ArbStep[], ArbStep[]));
        _migrateArb(buy, sell);
        _forceApprove(loanInFlight, address(MORPHO), assets);
    }

    /// @dev The PoolManager is unlocked for us: take the loan, run the arb, pay back exactly what we
    ///      took (sync -> transfer -> settle), so our delta nets to zero before unlock returns.
    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        if (msg.sender != address(POOL_MANAGER) || fundingInFlight != FUND_V4) revert Unauthorized();
        fundingInFlight = 0;
        (uint256 amount, ArbStep[] memory buy, ArbStep[] memory sell) =
            abi.decode(data, (uint256, ArbStep[], ArbStep[]));
        address loan = loanInFlight;
        POOL_MANAGER.take(loan, address(this), amount);
        _migrateArb(buy, sell);
        POOL_MANAGER.sync(loan);
        _safeTransfer(loan, address(POOL_MANAGER), amount);
        POOL_MANAGER.settle();
        return "";
    }

    function _begin(address loanToken, uint256 deadlineBlock, uint256 funding) private {
        if (msg.sender != executor) revert Unauthorized();
        if (block.number > deadlineBlock) revert Expired();
        if (paused) revert IsPaused();
        fundingInFlight = funding;
        loanInFlight = loanToken;
    }

    function _finish(address loanToken, uint256 loanAmount, uint256 minProfit) private returns (uint256 profit) {
        // The funder returned without calling back: nothing was borrowed, nothing was arbed.
        if (fundingInFlight != 0) revert NotFunded();
        profit = IERC20Bal(loanToken).balanceOf(address(this));
        if (profit < minProfit) revert InsufficientProfit(profit, minProfit);
        if (profit > 0) _safeTransfer(loanToken, PROFIT_RECEIVER, profit);
        // Dust the steps did not spend (exact-input routers, rounding).
        _sweep(OLD_TOKEN);
        _sweep(NEW_TOKEN);
        emit Arb(loanToken, loanAmount, migratedInFlight, profit);
    }

    /// @dev The arb itself, whatever funded it: loan token -> old AERO -> migrate -> new AERO ->
    ///      loan token. Repayment is the caller's job.
    function _migrateArb(ArbStep[] memory buy, ArbStep[] memory sell) private {
        _runSteps(0, buy);
        uint256 migrated = IERC20Bal(OLD_TOKEN).balanceOf(address(this));
        if (migrated == 0) revert NothingBought();
        _forceApprove(OLD_TOKEN, address(MIGRATION), migrated);
        MIGRATION.depositLiquid(migrated, address(this));
        _forceApprove(OLD_TOKEN, address(MIGRATION), 0);
        migratedInFlight = migrated;
        _runSteps(1, sell);
    }

    function _runSteps(uint256 leg, ArbStep[] memory steps) private {
        for (uint256 i; i < steps.length; ++i) {
            ArbStep memory s = steps[i];
            uint256 amount = s.amountIn == type(uint256).max ? IERC20Bal(s.tokenIn).balanceOf(address(this)) : s.amountIn;
            if (s.kind == 1 || s.kind == 2) {
                _poolSwap(s, amount);
                continue;
            }
            if (s.kind == 3) {
                _clSwap(s, amount);
                continue;
            }
            if (s.kind != 0) revert BadKind();
            if (!allowedTarget[s.target]) revert TargetNotAllowed(s.target);
            bytes memory d = s.data;
            if (s.patchOffset != 0) {
                uint256 off = s.patchOffset;
                if (off + 32 > d.length) revert BadPatch();
                assembly ("memory-safe") {
                    mstore(add(add(d, 32), off), amount)
                }
            }
            _forceApprove(s.tokenIn, s.target, amount);
            (bool ok, bytes memory ret) = s.target.call(d);
            if (!ok) revert SwapFailed(leg, i, ret);
            _forceApprove(s.tokenIn, s.target, 0);
        }
    }

    /// @dev Exact-input swap straight on a Solidly-style pool of an allowlisted factory. A fake
    ///      pool cannot pass: its factory() may claim an allowlisted factory, but that factory's
    ///      isPool() must confirm it.
    function _poolSwap(ArbStep memory s, uint256 amount) private {
        ISolidlyPool pool = ISolidlyPool(s.target);
        address f = pool.factory();
        if (!allowedTarget[f] || !ISolidlyFactory(f).isPool(s.target)) revert TargetNotAllowed(s.target);
        uint256 out = s.kind == 1 ? pool.getAmountOut(amount, s.tokenIn) : pool.getAmountOutWithTotalFee(amount, s.tokenIn);
        // 1 ppm below the quote: guards against fee rounding between the view and swap().
        out -= out / 1_000_000 + 1;
        _safeTransfer(s.tokenIn, s.target, amount);
        if (s.tokenIn == pool.token0()) pool.swap(0, out, address(this), "");
        else pool.swap(out, 0, address(this), "");
    }

    /// @dev Exact-input swap straight on a CL pool. A fake pool cannot pass: its factory() may claim
    ///      an allowlisted factory, but that factory's getPool must return this very address.
    function _clSwap(ArbStep memory s, uint256 amount) private {
        IV3Pool pool = IV3Pool(s.target);
        address f = IClPool(s.target).factory();
        address t0 = pool.token0();
        address t1 = pool.token1();
        if (!allowedTarget[f] || IClFactory(f).getPool(t0, t1, IClPool(s.target).tickSpacing()) != s.target) {
            revert TargetNotAllowed(s.target);
        }
        bool zeroForOne = s.tokenIn == t0;
        if (!zeroForOne && s.tokenIn != t1) revert TargetNotAllowed(s.target);
        activePool = s.target;
        activeToken = s.tokenIn;
        activeMaxIn = amount;
        pool.swap(address(this), zeroForOne, int256(amount), zeroForOne ? MIN_SQRT_PRICE_LIMIT : MAX_SQRT_PRICE_LIMIT, "");
        activePool = address(0);
    }

    /// @dev Pays the CL pool its input: only the pool swapping right now, once, at most its amountIn.
    function uniswapV3SwapCallback(int256 amount0, int256 amount1, bytes calldata) external {
        address pool = activePool;
        if (msg.sender != pool || pool == address(0)) revert Unauthorized();
        activePool = address(0);
        uint256 owed = uint256(amount0 > 0 ? amount0 : amount1);
        if (owed > activeMaxIn) revert Unauthorized();
        _safeTransfer(activeToken, pool, owed);
    }

    // ---------------------------------------------------------------- admin

    function setExecutor(address executor_) external onlyOwner {
        executor = executor_;
        emit ExecutorSet(executor_);
    }

    function setAllowedTarget(address target, bool allowed) external onlyOwner {
        allowedTarget[target] = allowed;
        emit TargetAllowed(target, allowed);
    }

    /// @notice Executor may pause (bot self-stop); only owner may resume.
    function pause() external {
        if (msg.sender != owner && msg.sender != executor) revert Unauthorized();
        paused = true;
        emit PausedSet(true);
    }

    function unpause() external onlyOwner {
        paused = false;
        emit PausedSet(false);
    }

    function transferOwnership(address newOwner) external onlyOwner {
        pendingOwner = newOwner;
        emit OwnershipTransferStarted(newOwner);
    }

    function acceptOwnership() external {
        if (msg.sender != pendingOwner) revert Unauthorized();
        owner = msg.sender;
        pendingOwner = address(0);
        emit OwnerSet(msg.sender);
    }

    function rescue(address token, address to, uint256 amount) external onlyOwner {
        _safeTransfer(token, to, amount);
    }

    // ---------------------------------------------------------------- internal

    function _sweep(address token) private {
        uint256 b = IERC20Bal(token).balanceOf(address(this));
        if (b > 0) _safeTransfer(token, PROFIT_RECEIVER, b);
    }

    function _forceApprove(address token, address spender, uint256 amount) private {
        if (!_tokenCall(token, abi.encodeWithSelector(0x095ea7b3, spender, amount))) {
            if (!_tokenCall(token, abi.encodeWithSelector(0x095ea7b3, spender, 0))) revert TokenCallFailed(token);
            if (!_tokenCall(token, abi.encodeWithSelector(0x095ea7b3, spender, amount))) revert TokenCallFailed(token);
        }
    }

    function _safeTransfer(address token, address to, uint256 amount) private {
        if (!_tokenCall(token, abi.encodeWithSelector(0xa9059cbb, to, amount))) revert TokenCallFailed(token);
    }

    function _tokenCall(address token, bytes memory data) private returns (bool) {
        (bool ok, bytes memory ret) = token.call(data);
        return ok && (ret.length == 0 || (ret.length == 32 && abi.decode(ret, (bool)))) && token.code.length > 0;
    }
}
