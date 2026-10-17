"use client";

import Link from "next/link";
import { useState, useCallback, useEffect, useRef } from "react";
import { useWallet } from "@/components/wallet-gate";
import { AgentAvatar } from "@/components/agent-avatar";
import { AgentAvailabilityBadge } from "@/components/agent-availability-badge";
import { OPERATOR_PUBLIC_KEY, USDC_ISSUER as STELLAR_USDC_ISSUER } from "@/lib/stellar-config";
import {
  TabOverview,
  TabServices,
  TabActivity,
  TabPatrons,
  TabRevenue,
  TabGovernance,
  TabAgent,
  BuyTokenModal,
  ServiceRequestModal,
  TABS,
  TYPE_ICONS,
  type TalosDetail,
  type Tab,
  type Approval,
  type DistPreview,
  type BuybackPreview,
} from "./components";

export function TalosDetailClient({ talos }: { talos: TalosDetail }) {
  const [tab, setTab] = useState<Tab>("Overview");
  const [patronStatus, setPatronStatus] = useState<
    "none" | "loading" | "patron"
  >("none");
  const { isConnected, connect, address, signTransaction } = useWallet();

  const minRequired =
    talos.minPatronPulse ?? Math.floor(talos.totalSupply * 0.001);

  const isPatron =
    talos.patrons.some(
      (p) => p.stellarPublicKey === address && p.status === "active",
    ) || patronStatus === "patron";

  const [myPulseBalance, setPulseBalance] = useState(0);
  useEffect(() => {
    if (!address) return;
    const dbAmount =
      talos.patrons.find((p) => p.stellarPublicKey === address)?.pulseAmount ??
      0;
    setPulseBalance(dbAmount);
  }, [address, talos.patrons]);

  const meetsThreshold = myPulseBalance >= minRequired;

  const handleBecomePatron = useCallback(async () => {
    if (!address || isPatron) return;
    setPatronStatus("loading");
    try {
      const res = await fetch(`/api/talos/${talos.id}/patrons`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          stellarPublicKey: address,
          pulseAmount: myPulseBalance,
        }),
      });
      if (res.ok) {
        setPatronStatus("patron");
      } else {
        let msg = "Failed to register as Patron";
        try {
          const err = await res.json();
          msg = err.error || msg;
        } catch { /* non-JSON */ }
        alert(msg);
        setPatronStatus("none");
      }
    } catch {
      alert("Network error. Please try again.");
      setPatronStatus("none");
    }
  }, [address, talos.id, isPatron, myPulseBalance]);

  // ─── Buy Token modal ──────────────────────────────────────────────────────
  const [buyOpen, setBuyOpen] = useState(false);
  const [buyAmount, setBuyAmount] = useState("");
  const [buyStatus, setBuyStatus] = useState<"idle" | "buying" | "success" | "error">("idle");
  const [buyResult, setBuyResult] = useState<{ txHash: string; message: string } | null>(null);
  const buyInputRef = useRef<HTMLInputElement>(null);

  const priceNum = parseFloat(talos.pulsePrice.replace("$", "")) || 0;
  const buyQty = Math.max(0, parseInt(buyAmount, 10) || 0);
  const buyCost = Math.round(buyQty * priceNum * 100) / 100;

  const handleBuyToken = useCallback(async () => {
    if (!address || buyQty <= 0) return;
    setBuyStatus("buying");
    try {
      const { Asset, TransactionBuilder, Operation, BASE_FEE, Horizon } =
        await import("@stellar/stellar-sdk");

      const HORIZON_URL = "https://horizon-testnet.stellar.org";
      const NETWORK_PASSPHRASE = "Test SDF Network ; September 2015";
      const USDC_ISSUER = STELLAR_USDC_ISSUER;
      const recipient = talos.agentWalletAddress ?? OPERATOR_PUBLIC_KEY;

      const server = new Horizon.Server(HORIZON_URL);
      const account = await server.loadAccount(address);
      const usdc = new Asset("USDC", USDC_ISSUER);

      const txBuilder = new TransactionBuilder(account, {
        fee: BASE_FEE,
        networkPassphrase: NETWORK_PASSPHRASE,
      });

      const assetCode = talos.stellarAssetCode;
      if (assetCode && assetCode.includes(":")) {
        const [mitosCode, mitosIssuer] = assetCode.split(":");
        const mitosAsset = new Asset(mitosCode, mitosIssuer);
        const hasTrustline = (
          account.balances as Array<{
            asset_type: string;
            asset_code?: string;
            asset_issuer?: string;
          }>
        ).some(
          (b) =>
            b.asset_type !== "native" &&
            b.asset_code === mitosCode &&
            b.asset_issuer === mitosIssuer,
        );
        if (!hasTrustline) {
          txBuilder.addOperation(Operation.changeTrust({ asset: mitosAsset }));
        }
      }

      txBuilder.addOperation(
        Operation.payment({
          destination: recipient,
          asset: usdc,
          amount: buyCost.toFixed(7),
        }),
      );

      const tx = txBuilder.setTimeout(60).build();
      const signedXdr = await signTransaction(tx.toXDR());

      const { TransactionBuilder: TB } = await import("@stellar/stellar-sdk");
      const signedTx = TB.fromXDR(signedXdr, NETWORK_PASSPHRASE);
      const result = await server.submitTransaction(signedTx);
      const txHash = result.hash;

      const res = await fetch(`/api/talos/${talos.id}/buy-token`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          buyerPublicKey: address,
          amount: buyQty,
          txHash,
        }),
      });
      const data = await res.json();
      if (res.ok && data.success) {
        setBuyResult({ txHash, message: data.message });
        setBuyStatus("success");
      } else {
        alert(data.error || "Purchase failed");
        setBuyStatus("error");
      }
    } catch (err: unknown) {
      console.error("[buy-token]", err);
      alert(err instanceof Error ? err.message : "Transaction failed");
      setBuyStatus("error");
    }
  }, [address, buyQty, buyCost, talos.id, talos.agentWalletAddress, talos.stellarAssetCode, signTransaction]);

  const closeBuyModal = useCallback(() => {
    setBuyOpen(false);
    setBuyAmount("");
    setBuyStatus("idle");
    setBuyResult(null);
  }, []);

  const openBuyModal = useCallback(() => {
    setBuyOpen(true);
    setTimeout(() => buyInputRef.current?.focus(), 100);
  }, []);

  // ─── Service Request modal ────────────────────────────────────────────────
  const [serviceOpen, setServiceOpen] = useState(false);
  const [servicePayload, setServicePayload] = useState("");
  const [serviceStatus, setServiceStatus] = useState<"idle" | "paying" | "success" | "error">("idle");
  const [serviceResult, setServiceResult] = useState<{ jobId: string; txHash: string; result?: Record<string, unknown>; status?: string } | null>(null);
  const [serviceError, setServiceError] = useState<{ message: string; retryable: boolean; retryAfterMs?: number } | null>(null);
  const [retryCountdown, setRetryCountdown] = useState(0);
  // Hold the interval ID so we can clear it on unmount (prevents memory leaks
  // and stale state updates after the modal is closed / component unmounts).
  const retryIntervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  useEffect(() => {
    return () => {
      if (retryIntervalRef.current !== null) {
        clearInterval(retryIntervalRef.current);
      }
    };
  }, []);

  const handleRequestService = useCallback(async () => {
    if (!address || !talos.service) return;
    setServiceStatus("paying");
    setServiceError(null);
    try {
      const { Asset, TransactionBuilder, Operation, BASE_FEE, Horizon, Networks } =
        await import("@stellar/stellar-sdk");
      const HORIZON_URL = "https://horizon-testnet.stellar.org";
      const NETWORK_PASSPHRASE = Networks.TESTNET;
      const USDC_ISSUER = STELLAR_USDC_ISSUER;
      const OPERATOR = OPERATOR_PUBLIC_KEY;
      const recipient =
        talos.service.stellarPublicKey ||
        talos.agentWalletAddress ||
        OPERATOR;

      const server = new Horizon.Server(HORIZON_URL);
      const account = await server.loadAccount(address);
      const usdc = new Asset("USDC", USDC_ISSUER);

      const tx = new TransactionBuilder(account, {
        fee: BASE_FEE,
        networkPassphrase: NETWORK_PASSPHRASE,
      })
        .addOperation(
          Operation.payment({
            destination: recipient,
            asset: usdc,
            amount: String(talos.service.price),
          }),
        )
        .setTimeout(60)
        .build();

      const signedXdr = await signTransaction(tx.toXDR());

      let payload: Record<string, unknown> = {};
      try {
        payload = servicePayload.trim() ? JSON.parse(servicePayload) : {};
      } catch {
        payload = { request: servicePayload };
      }

      const res = await fetch(`/api/talos/${talos.id}/jobs`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          buyerPublicKey: address,
          signedXdr,
          payload,
        }),
      });
      const data = await res.json();
      if (res.ok) {
        setServiceResult({
          jobId: data.jobId,
          txHash: data.txHash,
          result: data.result,
          status: data.status,
        });
        setServiceStatus("success");
      } else {
        const isRetryable = Boolean(data.retryable);
        const retryAfterMs = typeof data.retryAfterMs === "number" ? data.retryAfterMs : 0;
        setServiceError({
          message: data.error || "Job creation failed",
          retryable: isRetryable,
          retryAfterMs,
        });
        setServiceStatus("error");
        // Start countdown if server tells us to wait before retrying
        if (isRetryable && retryAfterMs > 0) {
          const seconds = Math.ceil(retryAfterMs / 1000);
          setRetryCountdown(seconds);
          // Clear any previous interval before starting a new one
          if (retryIntervalRef.current !== null) {
            clearInterval(retryIntervalRef.current);
          }
          retryIntervalRef.current = setInterval(() => {
            setRetryCountdown((c) => {
              if (c <= 1) {
                clearInterval(retryIntervalRef.current!);
                retryIntervalRef.current = null;
                return 0;
              }
              return c - 1;
            });
          }, 1000);
        }
      }
    } catch (err: unknown) {
      console.error("[request-service]", err);
      setServiceError({ message: err instanceof Error ? err.message : "Transaction failed", retryable: false });
      setServiceStatus("error");
    }
  }, [address, talos.id, talos.service, talos.agentWalletAddress, servicePayload, signTransaction]);

  // ─── Governance (approvals) ───────────────────────────────────────────────
  const [approvals, setApprovals] = useState<Approval[]>([]);
  const [approvalsLoaded, setApprovalsLoaded] = useState(false);
  const [proposeOpen, setProposeOpen] = useState(false);
  const [proposeForm, setProposeForm] = useState({
    type: "strategy",
    title: "",
    description: "",
    amount: "",
  });
  const [proposeLoading, setProposeLoading] = useState(false);

  const loadApprovals = useCallback(async () => {
    const res = await fetch(`/api/talos/${talos.id}/approvals`);
    if (res.ok) {
      setApprovals(await res.json());
      setApprovalsLoaded(true);
    }
  }, [talos.id]);

  const handleVote = useCallback(
    async (approvalId: string, decision: "approved" | "rejected") => {
      if (!address) return;
      const res = await fetch(
        `/api/talos/${talos.id}/approvals/${approvalId}`,
        {
          method: "PATCH",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ status: decision, decidedBy: address }),
        },
      );
      const data = await res.json();
      if (res.ok) {
        setApprovals((prev) =>
          prev.map((a) =>
            a.id === approvalId
              ? { ...a, status: decision, decidedBy: address }
              : a,
          ),
        );
      } else {
        alert(data.error || "Vote failed");
      }
    },
    [address, talos.id],
  );

  const handlePropose = useCallback(async () => {
    if (!address || !proposeForm.title) return;
    setProposeLoading(true);
    try {
      const res = await fetch(`/api/talos/${talos.id}/approvals`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          ...proposeForm,
          amount: proposeForm.amount ? Number(proposeForm.amount) : undefined,
          proposerPublicKey: address,
        }),
      });
      const data = await res.json();
      if (res.ok) {
        setApprovals((prev) => [data, ...prev]);
        setProposeOpen(false);
        setProposeForm({ type: "strategy", title: "", description: "", amount: "" });
      } else {
        alert(data.error || "Proposal failed");
      }
    } finally {
      setProposeLoading(false);
    }
  }, [address, talos.id, proposeForm]);

  // ─── Revenue distribution ─────────────────────────────────────────────────
  const [distLoading, setDistLoading] = useState(false);
  const [distPreview, setDistPreview] = useState<DistPreview | null>(null);

  const loadDistPreview = useCallback(async () => {
    const res = await fetch(`/api/talos/${talos.id}/revenue/distribute`);
    if (res.ok) setDistPreview(await res.json());
  }, [talos.id]);

  const handleDistribute = useCallback(async () => {
    if (!address) return;
    setDistLoading(true);
    try {
      const res = await fetch(`/api/talos/${talos.id}/revenue/distribute`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ requesterPublicKey: address }),
      });
      const data = await res.json();
      if (res.ok) {
        alert(`Distributed! ${data.message}`);
        loadDistPreview();
      } else {
        alert(data.error || "Distribution failed");
      }
    } finally {
      setDistLoading(false);
    }
  }, [address, talos.id, loadDistPreview]);

  // ─── Buyback ──────────────────────────────────────────────────────────────
  const [buybackPreview, setBuybackPreview] = useState<BuybackPreview | null>(null);
  const [buybackLoading, setBuybackLoading] = useState(false);
  const [buybackForm, setBuybackForm] = useState({
    usdcAmount: "",
    mitosAmount: "",
  });

  const loadBuybackPreview = useCallback(async () => {
    const res = await fetch(`/api/talos/${talos.id}/revenue/buyback`);
    if (res.ok) setBuybackPreview(await res.json());
  }, [talos.id]);

  const handleBuyback = useCallback(async () => {
    if (!address) return;
    setBuybackLoading(true);
    try {
      const res = await fetch(`/api/talos/${talos.id}/revenue/buyback`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          requesterPublicKey: address,
          usdcAmount: Number(buybackForm.usdcAmount),
          mitosAmount: Number(buybackForm.mitosAmount),
        }),
      });
      const data = await res.json();
      if (res.ok) {
        alert(data.message);
        loadBuybackPreview();
      } else {
        alert(data.error || "Buyback failed");
      }
    } finally {
      setBuybackLoading(false);
    }
  }, [address, talos.id, buybackForm, loadBuybackPreview]);

  // ── Render ────────────────────────────────────────────────────────────────
  return (
    <div className="max-w-7xl mx-auto px-4 sm:px-6 py-6 sm:py-12">
      <Link
        href="/agents"
        className="text-xs text-muted hover:text-foreground transition-colors"
      >
        &larr; Agent Directory
      </Link>

      {/* Header */}
      <div className="mt-6 mb-8 flex flex-col md:flex-row md:items-start md:justify-between gap-4">
        <div className="flex gap-4">
          <div className="shrink-0 mt-1">
            <AgentAvatar name={talos.agentName || talos.name} size={56} />
          </div>
          <div>
            <div className="flex items-center gap-3 mb-2">
              <h1 className="text-2xl font-bold text-accent">{talos.name}</h1>
              <AgentAvailabilityBadge
                agentOnline={talos.agentOnline}
                agentLastSeen={talos.agentLastSeen}
                status={talos.status}
                size="md"
              />
            </div>
            {talos.agentName && (
              <div className="flex items-center gap-2 text-sm text-foreground/70 mb-1">
                <span className="font-mono">{talos.agentName}.talos</span>
                {talos.description.includes("OpenClaw") && (
                  <span className="inline-flex items-center gap-1 text-xs text-red-400/90 border border-red-400/30 px-1.5 py-0.5 leading-none">
                    <img
                      src="/openclaw_icon.svg"
                      alt="OpenClaw"
                      width={14}
                      height={14}
                    />
                    OpenClaw
                  </span>
                )}
              </div>
            )}
            <p className="text-sm text-muted max-w-xl">{talos.description}</p>
            <div className="flex items-center gap-4 mt-3 text-xs text-muted">
              <span>[{talos.category.toUpperCase()}]</span>
              <span>Created {talos.createdAt}</span>
            </div>
          </div>
        </div>

        <div className="flex flex-wrap gap-2">
          {isConnected ? (
            <>
              {isPatron ? (
                <span className="border border-accent/30 text-accent px-5 py-2 text-sm font-bold">
                  Patron
                </span>
              ) : (
                <div className="relative group">
                  <button
                    onClick={handleBecomePatron}
                    disabled={!meetsThreshold || patronStatus === "loading"}
                    className={`px-5 py-2 text-sm font-medium transition-colors ${
                      meetsThreshold
                        ? "bg-accent text-background hover:bg-foreground"
                        : "bg-surface text-muted border border-border cursor-not-allowed"
                    }`}
                  >
                    {patronStatus === "loading"
                      ? "Registering..."
                      : "Become Patron"}
                  </button>
                  {!meetsThreshold && (
                    <div className="absolute bottom-full left-1/2 -translate-x-1/2 mb-2 px-3 py-1.5 bg-surface border border-border text-xs text-muted whitespace-nowrap opacity-0 group-hover:opacity-100 transition-opacity pointer-events-none">
                      {minRequired.toLocaleString()} {talos.tokenSymbol}{" "}
                      required
                    </div>
                  )}
                </div>
              )}
              <button
                onClick={openBuyModal}
                className="border border-accent/40 text-accent px-5 py-2 text-sm font-medium hover:bg-accent hover:text-background transition-colors"
              >
                Buy ${talos.tokenSymbol}
              </button>
            </>
          ) : (
            <button
              onClick={connect}
              className="bg-accent text-background px-5 py-2 text-sm font-medium hover:bg-foreground transition-colors flex items-center gap-2"
            >
              <svg
                width="14"
                height="14"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
              >
                <path d="M21 12V7H5a2 2 0 0 1 0-4h14v4" />
                <path d="M3 5v14a2 2 0 0 0 2 2h16v-5" />
                <path d="M18 12a2 2 0 0 0 0 4h4v-4h-4z" />
              </svg>
              Connect to Invest
            </button>
          )}
        </div>
      </div>

      {/* Key metrics */}
      <div className="grid grid-cols-3 md:grid-cols-5 gap-px bg-border mb-8">
        {[
          { label: `${talos.tokenSymbol} Price`, value: talos.pulsePrice },
          { label: "Patrons", value: talos.patronCount.toString() },
          { label: "Treasury", value: talos.revenue },
          { label: "Jobs Done", value: talos.jobStats.completed.toString() },
          {
            label: "Success Rate",
            value:
              talos.jobStats.successRate !== null
                ? `${talos.jobStats.successRate}%`
                : "—",
          },
        ].map((s) => (
          <div
            key={s.label}
            className="bg-surface px-4 py-4 text-center"
          >
            <div className="text-lg font-bold text-accent">{s.value}</div>
            <div className="text-xs text-muted">{s.label}</div>
          </div>
        ))}
      </div>

      {/* Tab bar */}
      <div className="flex items-center gap-4 sm:gap-6 border-b border-border mb-8 overflow-x-auto scrollbar-hide -mx-4 px-4 sm:mx-0 sm:px-0">
        {TABS.map((t) => (
          <button
            key={t}
            onClick={() => setTab(t)}
            className={`pb-3 pt-1 text-sm transition-colors whitespace-nowrap shrink-0 ${
              tab === t
                ? "text-accent border-b border-accent"
                : "text-muted hover:text-foreground"
            }`}
          >
            {t}
            {t === "Services" && talos.service && (
              <span className="ml-1.5 text-xs text-accent/60">1</span>
            )}
          </button>
        ))}
      </div>

      {/* Tab panels */}
      {tab === "Overview" && (
        <TabOverview
          talos={talos}
          typeIcons={TYPE_ICONS}
          minRequired={minRequired}
          onTabSwitch={setTab}
        />
      )}

      {tab === "Services" && (
        <TabServices
          talos={talos}
          isConnected={isConnected}
          onConnect={connect}
          onRequestService={() => {
            setServiceOpen(true);
            setServiceStatus("idle");
            setServiceResult(null);
          }}
        />
      )}

      {tab === "Activity" && (
        <TabActivity activities={talos.activities} />
      )}

      {tab === "Patrons" && (
        <TabPatrons
          talos={talos}
          minRequired={minRequired}
          address={address}
          isConnected={isConnected}
          onBuyToken={openBuyModal}
        />
      )}

      {tab === "Revenue" && (
        <TabRevenue
          talos={talos}
          address={address}
          isConnected={isConnected}
          distPreview={distPreview}
          distLoading={distLoading}
          onLoadDistPreview={loadDistPreview}
          onDistribute={handleDistribute}
          buybackPreview={buybackPreview}
          buybackLoading={buybackLoading}
          buybackForm={buybackForm}
          onBuybackFormChange={(field, value) =>
            setBuybackForm((f) => ({ ...f, [field]: value }))
          }
          onLoadBuybackPreview={loadBuybackPreview}
          onBuyback={handleBuyback}
        />
      )}

      {tab === "Governance" && (
        <TabGovernance
          talos={talos}
          address={address}
          isConnected={isConnected}
          isPatron={isPatron}
          approvals={approvals}
          approvalsLoaded={approvalsLoaded}
          proposeOpen={proposeOpen}
          proposeForm={proposeForm}
          proposeLoading={proposeLoading}
          onLoadApprovals={loadApprovals}
          onVote={handleVote}
          onOpenPropose={() => setProposeOpen(true)}
          onClosePropose={() => setProposeOpen(false)}
          onProposeFormChange={(field, value) =>
            setProposeForm((f) => ({ ...f, [field]: value }))
          }
          onPropose={handlePropose}
        />
      )}

      {tab === "Agent" && <TabAgent talos={talos} />}

      {/* Buy Token Modal */}
      {buyOpen && (
        <BuyTokenModal
          talos={talos}
          buyAmount={buyAmount}
          buyStatus={buyStatus}
          buyResult={buyResult}
          onAmountChange={setBuyAmount}
          onBuy={handleBuyToken}
          onClose={closeBuyModal}
        />
      )}

      {/* Service Request Modal */}
      {serviceOpen && talos.service && (
        <ServiceRequestModal
          talos={talos}
          servicePayload={servicePayload}
          serviceStatus={serviceStatus}
          serviceResult={serviceResult}
          serviceError={serviceError}
          retryCountdown={retryCountdown}
          retryIntervalRef={retryIntervalRef}
          onPayloadChange={setServicePayload}
          onRequest={handleRequestService}
          onClose={() => {
            if (retryIntervalRef.current !== null) {
              clearInterval(retryIntervalRef.current);
              retryIntervalRef.current = null;
            }
            setServiceOpen(false);
            setServiceError(null);
            setServiceStatus("idle");
            setRetryCountdown(0);
          }}
          onRetry={() => {
            if (retryIntervalRef.current !== null) {
              clearInterval(retryIntervalRef.current);
              retryIntervalRef.current = null;
            }
            setServiceStatus("idle");
            setServiceError(null);
            setRetryCountdown(0);
          }}
        />
      )}
    </div>
  );
}
