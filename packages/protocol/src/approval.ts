import { z } from 'zod';

import { ToolInputDisplaySchema } from './display';
import { isoDateTimeSchema } from './time';

export const approvalDecisionSchema = z.enum(['approved', 'rejected', 'cancelled']);
export type ApprovalDecision = z.infer<typeof approvalDecisionSchema>;

export const approvalScopeSchema = z.enum(['session']);
export type ApprovalScope = z.infer<typeof approvalScopeSchema>;

export const approvalRequestSchema = z.object({
  approval_id: z.string().min(1),
  session_id: z.string().min(1),
  turn_id: z.number().int().nonnegative().optional(),
  tool_call_id: z.string().min(1),
  tool_name: z.string().min(1),
  action: z.string(),
  // Canonical wire dialect only (ADR-011): legacy v1 aliases stay
  // tolerated in the web presentation mapper, never in the contract.
  tool_input_display: ToolInputDisplaySchema,
  created_at: isoDateTimeSchema,
  expires_at: isoDateTimeSchema,
});
export type ApprovalRequest = z.infer<typeof approvalRequestSchema>;

export const approvalResponseSchema = z.object({
  decision: approvalDecisionSchema,
  scope: approvalScopeSchema.optional(),
  feedback: z.string().optional(),
  selected_label: z.string().optional(),
});
export type ApprovalResponse = z.infer<typeof approvalResponseSchema>;
