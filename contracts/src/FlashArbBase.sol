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

/// @notice One step; `amountIn` = type(uint256).max spends this contract's whole balance of
///         `tokenIn` (a balance only known on-chain, e.g. what an earlier step paid out).
///         kind 0 (router / any allowlisted target): `target` gets an exact allowance of `tokenIn`
///           and is called with `data`; when `patchOffset` is non-zero, the amount is also written
///           into `data` at that byte offset (the target's amountIn field).
///         kind 1 / 2 (Solidly-style pool, direct): `target` is the pool. It must belong to an
///           allowlisted factory (`isPool`); the input is sent to it and the output computed with
///           `getAmountOut` (1: legacy Aerodrome) or `getAmountOutWithTotalFee` (2: Aero pools,
///           dynamic + MEV fee), in the same tx, so no quote can go stale. `data` is unused.
///         kind 3 (CL pool, direct: Uniswap V3 / Slipstream / Aero CL): `target` is the pool. Its
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

/// @title FlashArbBase
/// @notice What every flash-funded atomic arb contract here shares: the two funding sources, the
///         steps, the security model and the admin. A contract adds its arb body (`_body`), its
///         entry points (which call `_flash` and `_finish`) and what it sweeps after.
/// @dev Funding: `_flash(FUND_MORPHO, ...)` = Morpho Blue flash loan (0 fee); `FUND_V4` = Uniswap V4
///      PoolManager take + settle (0 fee; swap steps must not go through V4 then: the manager is
///      already unlocked by us). Security model:
///      - only `executor` (hot wallet) can start; only `owner` (cold wallet) changes settings,
///        unpauses or moves funds; two-step ownership;
///      - each funding callback only runs inside our own arb funded by that very source, once
///        (transient funding tag + the funder as sender); an entry whose callback never ran reverts;
///      - kind-0 steps may only call owner-allowlisted targets (`allowedTarget`), with exact
///        allowances reset to zero; pool steps (kinds 1-3) only pools of owner-allowlisted
///        factories (`allowedFactory`), verified against the factory. Two lists, so a factory is
///        never a kind-0 target (approve + arbitrary call);
///      - Morpho gets an exact allowance for the repayment; the PoolManager is paid exactly what we
///        took;
///      - the tx reverts unless the loan-token balance left after repayment is at least
///        `minProfit`; profit goes to the immutable PROFIT_RECEIVER (the contract holds nothing at
///        rest: each contract sweeps its intermediate tokens).
abstract contract FlashArbBase {
    uint256 internal constant FUND_MORPHO = 1;
    uint256 internal constant FUND_V4 = 2;

    IMorphoFlash public immutable MORPHO;
    /// @dev address(0): flash accounting disabled.
    IPoolManager public immutable POOL_MANAGER;
    address public immutable PROFIT_RECEIVER;

    address public owner;
    address public executor;
    bool public paused;
    address public pendingOwner;
    /// @notice Targets kind-0 steps may call.
    mapping(address => bool) public allowedTarget;
    /// @notice Pool factories whose pools kind 1-3 steps may swap on.
    mapping(address => bool) public allowedFactory;

    /// @dev Which funder may call back right now (FUND_*); 0 = none / already called back.
    uint256 private transient fundingInFlight;
    address internal transient loanInFlight;
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
    error BadPatch();
    error BadKind();
    error FundingDisabled();
    error NotFunded();

    event ExecutorSet(address executor);
    event OwnershipTransferStarted(address indexed pendingOwner);
    event OwnerSet(address owner);
    event PausedSet(bool paused);
    event TargetAllowed(address indexed target, bool allowed);
    event FactoryAllowed(address indexed factory, bool allowed);

    modifier onlyOwner() {
        if (msg.sender != owner) revert Unauthorized();
        _;
    }

    constructor(IMorphoFlash morpho, IPoolManager poolManager, address owner_, address executor_, address profitReceiver_) {
        MORPHO = morpho;
        POOL_MANAGER = poolManager;
        PROFIT_RECEIVER = profitReceiver_;
        owner = owner_;
        executor = executor_;
        emit OwnerSet(owner_);
        emit ExecutorSet(executor_);
    }

    /// @dev The arb itself, whatever funded it (`body` = what the entry point passed to `_flash`).
    ///      Repayment is not its job.
    function _body(bytes memory body) internal virtual;

    /// @dev Checks the caller, the deadline and the pause, then borrows `loanAmount` of `loanToken`
    ///      from `funding` and runs `_body(body)` inside its callback.
    function _flash(uint256 funding, address loanToken, uint256 loanAmount, bytes memory body, uint256 deadlineBlock) internal {
        if (msg.sender != executor) revert Unauthorized();
        if (block.number > deadlineBlock) revert Expired();
        if (paused) revert IsPaused();
        if (funding == FUND_V4 && address(POOL_MANAGER) == address(0)) revert FundingDisabled();
        fundingInFlight = funding;
        loanInFlight = loanToken;
        if (funding == FUND_MORPHO) MORPHO.flashLoan(loanToken, loanAmount, body);
        else POOL_MANAGER.unlock(abi.encode(loanAmount, body));
    }

    /// @dev Morpho sent `assets` of the loan token; it pulls them back right after this returns.
    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external {
        if (msg.sender != address(MORPHO) || fundingInFlight != FUND_MORPHO) revert Unauthorized();
        fundingInFlight = 0;
        _body(data);
        _forceApprove(loanInFlight, address(MORPHO), assets);
    }

    /// @dev The PoolManager is unlocked for us: take the loan, run the arb, pay back exactly what we
    ///      took (sync -> transfer -> settle), so our delta nets to zero before unlock returns.
    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        if (msg.sender != address(POOL_MANAGER) || fundingInFlight != FUND_V4) revert Unauthorized();
        fundingInFlight = 0;
        (uint256 amount, bytes memory body) = abi.decode(data, (uint256, bytes));
        address loan = loanInFlight;
        POOL_MANAGER.take(loan, address(this), amount);
        _body(body);
        POOL_MANAGER.sync(loan);
        _safeTransfer(loan, address(POOL_MANAGER), amount);
        POOL_MANAGER.settle();
        return "";
    }

    /// @dev After the flash: the loan-token balance left is the profit; at least `minProfit`, sent to
    ///      PROFIT_RECEIVER.
    function _finish(address loanToken, uint256 minProfit) internal returns (uint256 profit) {
        // The funder returned without calling back: nothing was borrowed, nothing was arbed.
        if (fundingInFlight != 0) revert NotFunded();
        profit = IERC20Bal(loanToken).balanceOf(address(this));
        if (profit < minProfit) revert InsufficientProfit(profit, minProfit);
        if (profit > 0) _safeTransfer(loanToken, PROFIT_RECEIVER, profit);
    }

    function _runSteps(uint256 leg, ArbStep[] memory steps) internal {
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
        if (!allowedFactory[f] || !ISolidlyFactory(f).isPool(s.target)) revert TargetNotAllowed(s.target);
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
        if (!allowedFactory[f] || IClFactory(f).getPool(t0, t1, IClPool(s.target).tickSpacing()) != s.target) {
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

    function setAllowedFactory(address factory, bool allowed) external onlyOwner {
        allowedFactory[factory] = allowed;
        emit FactoryAllowed(factory, allowed);
    }

    /// @notice The executor (hot wallet) or the owner may pause, e.g. an operator stop with the hot
    ///         key at hand (the bot itself never calls it; its kill switch is a file); only owner may
    ///         resume.
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

    function _sweep(address token) internal {
        uint256 b = IERC20Bal(token).balanceOf(address(this));
        if (b > 0) _safeTransfer(token, PROFIT_RECEIVER, b);
    }

    function _forceApprove(address token, address spender, uint256 amount) internal {
        if (!_tokenCall(token, abi.encodeWithSelector(0x095ea7b3, spender, amount))) {
            if (!_tokenCall(token, abi.encodeWithSelector(0x095ea7b3, spender, 0))) revert TokenCallFailed(token);
            if (!_tokenCall(token, abi.encodeWithSelector(0x095ea7b3, spender, amount))) revert TokenCallFailed(token);
        }
    }

    function _safeTransfer(address token, address to, uint256 amount) internal {
        if (!_tokenCall(token, abi.encodeWithSelector(0xa9059cbb, to, amount))) revert TokenCallFailed(token);
    }

    function _tokenCall(address token, bytes memory data) private returns (bool) {
        (bool ok, bytes memory ret) = token.call(data);
        return ok && (ret.length == 0 || (ret.length == 32 && abi.decode(ret, (bool)))) && token.code.length > 0;
    }
}
