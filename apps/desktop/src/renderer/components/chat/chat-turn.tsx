/**
 * Native-first chat turn renderer.
 * Transcript rows are Native ItemEnvelope fields — no Message/Part dual.
 * UI is intentionally rough while first-party Desktop cuts over.
 */
import {
	Message,
	MessageContent,
	MessageResponse,
} from "@devo/ui/components/ai-elements/message"
import {
	assistantOrReasoningText,
	isUserMessageItem,
	nativeItemType,
	userMessageText,
} from "@devo-ai/sdk/v2/client"
import { BotIcon, ChevronDownIcon, ChevronRightIcon, CopyIcon, SplitIcon } from "lucide-react"
import { memo, useCallback, useMemo, useState } from "react"
import type { ChatMessageEntry, ChatTurn as ChatTurnType } from "../../hooks/use-session-chat"
import type { SessionCompactionStatus } from "../../atoms/compaction"
import type { ProviderErrorEntry, ProviderRetryStatus } from "../../atoms/sessions"
import type { Agent } from "../../lib/types"
import { itemDisplayText } from "../../atoms/derived/session-chat"
import { UserMessageBlock } from "./user-message-block"
import { ProviderErrorRow } from "./provider-error-row"

export function formatTimestamp(ms: number): string {
	const date = new Date(ms)
	return date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
}

export function isSyntheticMessage(entry: ChatMessageEntry): boolean {
	const type = nativeItemType(entry.info)
	return type === "contextCompaction" || type === "plan"
}

function getUserText(entry: ChatMessageEntry): string {
	return userMessageText(entry.info)
}

function parseCreatedMs(envelope: ChatMessageEntry["info"]): number {
	const raw = envelope.createdAt
	const ms = Date.parse(String(raw ?? ""))
	return Number.isFinite(ms) ? ms : 0
}

interface ChatTurnProps {
	turn: ChatTurnType
	isLast: boolean
	isWorking: boolean
	agent?: Agent | null
	isConnected?: boolean
	compactionStatus?: SessionCompactionStatus
	retryStatus?: ProviderRetryStatus
	providerErrors?: ProviderErrorEntry[]
	onForkFromTurn?: (turnId: string) => void
	onEditUserMessage?: (messageId: string, text: string) => void
}

function NativeItemRow({ entry, streaming }: { entry: ChatMessageEntry; streaming?: boolean }) {
	const type = nativeItemType(entry.info)
	const text = itemDisplayText(entry.info)
	const state = entry.info.state

	if (type === "reasoning") {
		return (
			<details className="text-xs text-muted-foreground">
				<summary className="cursor-pointer select-none">Thinking</summary>
				<pre className="mt-1 whitespace-pre-wrap font-sans text-[12px] opacity-80">{text}</pre>
			</details>
		)
	}

	if (
		type === "toolCall" ||
		type === "toolResult" ||
		type === "commandExecution" ||
		type === "fileChange" ||
		type === "hostedToolCall"
	) {
		const title =
			String(entry.info.item.toolName ?? entry.info.item.command ?? type) +
			(state && state !== "completed" ? ` · ${state}` : "")
		return (
			<details className="rounded border border-border/60 bg-muted/30 px-2 py-1 text-xs">
				<summary className="cursor-pointer select-none font-medium">{title}</summary>
				<pre className="mt-1 max-h-48 overflow-auto whitespace-pre-wrap font-mono text-[11px] opacity-90">
					{typeof entry.info.item.output === "string"
						? entry.info.item.output
						: typeof entry.info.item.displayContent === "string"
							? entry.info.item.displayContent
							: JSON.stringify(entry.info.item.input ?? entry.info.item, null, 2)}
				</pre>
			</details>
		)
	}

	if (type === "contextCompaction") {
		return (
			<div className="text-center text-[11px] text-muted-foreground">
				{text || "Context compaction"} · {state}
			</div>
		)
	}

	if (type === "plan") {
		return (
			<div className="rounded border border-border/50 px-3 py-2 text-sm whitespace-pre-wrap">
				{text || "Plan"}
			</div>
		)
	}

	if (type === "assistantMessage" || !type) {
		if (!text) return null
		return (
			<Message from="assistant">
				<MessageContent>
					<MessageResponse streaming={streaming}>{text}</MessageResponse>
				</MessageContent>
			</Message>
		)
	}

	if (!text) return null
	return (
		<div className="text-sm text-muted-foreground whitespace-pre-wrap">
			<span className="mr-2 text-[10px] uppercase tracking-wide opacity-60">{type}</span>
			{text}
		</div>
	)
}

function areTurnsEqual(a: ChatTurnType, b: ChatTurnType): boolean {
	if (a.id !== b.id || a.turnId !== b.turnId) return false
	if (a.userMessage.info.revision !== b.userMessage.info.revision) return false
	if (a.assistantMessages.length !== b.assistantMessages.length) return false
	for (let i = 0; i < a.assistantMessages.length; i++) {
		const left = a.assistantMessages[i].info
		const right = b.assistantMessages[i].info
		if (left.id !== right.id || left.revision !== right.revision || left.state !== right.state) {
			return false
		}
		if (assistantOrReasoningText(left).length !== assistantOrReasoningText(right).length) {
			return false
		}
	}
	return true
}

export const ChatTurnComponent = memo(
	function ChatTurnComponent({
		turn,
		isLast,
		isWorking,
		providerErrors = [],
		onForkFromTurn,
		onEditUserMessage,
	}: ChatTurnProps) {
		const [copied, setCopied] = useState(false)
		const [expanded, setExpanded] = useState(true)
		const isSynthetic = useMemo(() => isSyntheticMessage(turn.userMessage), [turn.userMessage])
		const userText = useMemo(() => getUserText(turn.userMessage), [turn.userMessage])
		const createdMs = parseCreatedMs(turn.userMessage.info)

		const responseText = useMemo(() => {
			for (let i = turn.assistantMessages.length - 1; i >= 0; i--) {
				const entry = turn.assistantMessages[i]
				if (nativeItemType(entry.info) === "assistantMessage") {
					return assistantOrReasoningText(entry.info)
				}
			}
			return ""
		}, [turn.assistantMessages])

		const onCopy = useCallback(async () => {
			if (!responseText) return
			await navigator.clipboard.writeText(responseText)
			setCopied(true)
			setTimeout(() => setCopied(false), 1200)
		}, [responseText])

		return (
			<div className="group/turn flex flex-col gap-3 py-3" data-turn-id={turn.turnId ?? turn.id}>
				{isUserMessageItem(turn.userMessage.info) && !isSynthetic && (
					<UserMessageBlock
						text={userText}
						canEdit={Boolean(onEditUserMessage)}
						onEdit={
							onEditUserMessage
								? async (next) => {
										await onEditUserMessage(turn.userMessage.info.id, next)
									}
								: undefined
						}
					/>
				)}
				{isSynthetic && (
					<div className="text-center text-[11px] text-muted-foreground">
						{itemDisplayText(turn.userMessage.info)}
					</div>
				)}

				{turn.assistantMessages.length > 0 && (
					<div className="flex flex-col gap-2">
						<button
							type="button"
							className="flex items-center gap-1 self-start text-[11px] text-muted-foreground"
							onClick={() => setExpanded((v) => !v)}
						>
							{expanded ? (
								<ChevronDownIcon className="size-3.5 stroke-[1.5]" />
							) : (
								<ChevronRightIcon className="size-3.5 stroke-[1.5]" />
							)}
							<BotIcon className="size-3.5 stroke-[1.5]" />
							<span>
								{turn.assistantMessages.length} item
								{turn.assistantMessages.length === 1 ? "" : "s"}
								{isWorking && isLast ? " · working" : ""}
							</span>
						</button>
						{expanded &&
							turn.assistantMessages.map((entry) => (
								<NativeItemRow
									key={entry.info.id}
									entry={entry}
									streaming={isWorking && isLast && entry.info.state === "running"}
								/>
							))}
					</div>
				)}

				{providerErrors.map((row) => (
					<ProviderErrorRow key={row.id} entry={row} />
				))}

				<div className="flex items-center gap-1 opacity-0 transition-opacity group-hover/turn:opacity-100">
					{responseText && (
						<button
							type="button"
							className="inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[11px] text-muted-foreground hover:bg-muted"
							onClick={() => void onCopy()}
						>
							<CopyIcon className="size-3.5 stroke-[1.5]" />
							{copied ? "Copied" : "Copy"}
						</button>
					)}
					{onForkFromTurn && turn.turnId && (
						<button
							type="button"
							className="inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[11px] text-muted-foreground hover:bg-muted"
							onClick={() => onForkFromTurn(turn.turnId!)}
						>
							<SplitIcon className="size-3.5 stroke-[1.5]" />
							Fork
						</button>
					)}
				</div>
			</div>
		)
	},
	(prev, next) =>
		areTurnsEqual(prev.turn, next.turn) &&
		prev.isLast === next.isLast &&
		prev.isWorking === next.isWorking &&
		prev.providerErrors === next.providerErrors,
)
