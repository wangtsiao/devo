import type { SessionCompactionStatus } from "../../atoms/compaction"
import type { ReasoningPart, ToolPart } from "../../lib/types"
import { getToolCategory, type ToolCategory } from "./tool-category"

export type ProcessTimelineInput =
	| { kind: "tool"; part: ToolPart }
	| { kind: "text"; id: string; text: string; metadata?: Record<string, unknown> }
	| { kind: "reasoning"; part: ReasoningPart }
	| { kind: "compaction"; id: string; status: SessionCompactionStatus }

export type ProcessTimelineItem =
	| { kind: "text"; id: string; text: string; metadata?: Record<string, unknown> }
	| { kind: "thought"; part: ReasoningPart }
	| { kind: "tool"; part: ToolPart }
	| { kind: "tool-group"; category: ToolCategory; tools: ToolPart[] }
	| { kind: "compaction"; id: string; status: SessionCompactionStatus }

/**
 * Builds an interleaved process timeline from ordered assistant parts.
 * Each reasoning part becomes its own thought row; tools are grouped only when
 * consecutive and share the same category. Compaction markers stay in arrival
 * order among tools/reasoning/text.
 */
export function buildProcessTimeline(ordered: ProcessTimelineInput[]): ProcessTimelineItem[] {
	const items: ProcessTimelineItem[] = []
	let currentGroup: { category: ToolCategory; tools: ToolPart[] } | null = null

	const flushGroup = () => {
		if (!currentGroup) return
		if (currentGroup.tools.length === 1) {
			items.push({ kind: "tool", part: currentGroup.tools[0] })
		} else {
			items.push({
				category: currentGroup.category,
				kind: "tool-group",
				tools: currentGroup.tools,
			})
		}
		currentGroup = null
	}

	for (const part of ordered) {
		if (part.kind === "reasoning") {
			flushGroup()
			items.push({ kind: "thought", part: part.part })
		} else if (part.kind === "tool") {
			const category = getToolCategory(part.part.tool)
			if (currentGroup && currentGroup.category === category) {
				currentGroup.tools.push(part.part)
			} else {
				flushGroup()
				currentGroup = { category, tools: [part.part] }
			}
		} else if (part.kind === "text") {
			flushGroup()
			items.push({
				id: part.id,
				kind: "text",
				metadata: part.metadata,
				text: part.text,
			})
		} else if (part.kind === "compaction") {
			flushGroup()
			items.push({ kind: "compaction", id: part.id, status: part.status })
		}
	}

	flushGroup()
	return items
}

export function isReasoningPartActivelyStreaming(
	orderedParts: ProcessTimelineInput[],
	reasoningPart: ReasoningPart,
): boolean {
	if (reasoningPart.time.end) return false

	const partIndex = orderedParts.findIndex(
		(part) => part.kind === "reasoning" && part.part.id === reasoningPart.id,
	)
	if (partIndex === -1) return false

	for (let i = partIndex + 1; i < orderedParts.length; i++) {
		const part = orderedParts[i]
		if (part.kind === "text" && part.text.replace("[REDACTED]", "").trim()) {
			return false
		}
		if (part.kind === "tool" || part.kind === "reasoning" || part.kind === "compaction") {
			return false
		}
	}

	return true
}

export function processTimelineRowId(item: ProcessTimelineItem, index: number): string {
	switch (item.kind) {
		case "text":
			return item.id
		case "thought":
			return item.part.id
		case "tool":
			return item.part.id
		case "compaction":
			return item.id
		case "tool-group": {
			const toolIds = item.tools.map((tool) => tool.id).join("+")
			return `group-${item.category}-${toolIds || index}`
		}
	}
}
