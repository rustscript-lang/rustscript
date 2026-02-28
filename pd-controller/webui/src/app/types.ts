import type { Edge, Node } from "@xyflow/react";

export type Section = "edges" | "programs" | "debug_sessions";
export type SourceFlavor = "rustscript" | "javascript" | "lua" | "scheme";
export type UiInputType = "text" | "number";

export type UiBlockInput = {
  key: string;
  label: string;
  input_type: UiInputType;
  default_value: string;
  placeholder: string;
  connectable: boolean;
};

export type UiBlockOutput = {
  key: string;
  label: string;
  expr_from_input: string | null;
};

export type UiBlockDefinition = {
  id: string;
  title: string;
  category: string;
  description: string;
  inputs: UiBlockInput[];
  outputs: UiBlockOutput[];
  accepts_flow: boolean;
};

export type UiBlocksResponse = { blocks: UiBlockDefinition[] };

export type UiSourceBundle = {
  rustscript: string;
  javascript: string;
  lua: string;
  scheme: string;
};

export const initialSource: UiSourceBundle = {
  rustscript: "use vm;\n",
  javascript: "import * as vm from \"vm\";\n",
  lua: "local vm = require(\"vm\")\n",
  scheme: "(require (prefix-in vm. \"vm\"))\n"
};

export type UiRenderResponse = { source: UiSourceBundle };

export type UiGraphNode = {
  id: string;
  block_id: string;
  values: Record<string, string>;
  position?: { x: number; y: number };
};

export type UiGraphEdge = {
  source: string;
  source_output: string;
  target: string;
  target_input: string;
};

export type UiGraphEdgeWire = UiGraphEdge & {
  sourceHandle?: string;
  targetHandle?: string;
  data?: {
    source_output?: string;
    target_input?: string;
  };
};

export type ProgramSummary = {
  program_id: string;
  name: string;
  latest_version: number;
  versions: number;
  created_unix_ms: number;
  updated_unix_ms: number;
};

export type ProgramVersionSummary = {
  version: number;
  created_unix_ms: number;
  flavor: string;
  flow_synced: boolean;
};

export type ProgramListResponse = { programs: ProgramSummary[] };

export type ProgramDetailResponse = {
  program_id: string;
  name: string;
  latest_version: number;
  created_unix_ms: number;
  updated_unix_ms: number;
  versions: ProgramVersionSummary[];
};

export type ProgramVersionDetail = {
  version: number;
  created_unix_ms: number;
  flavor: string;
  flow_synced: boolean;
  nodes: UiGraphNode[];
  edges: UiGraphEdge[];
  source: UiSourceBundle;
};

export type ProgramVersionResponse = {
  program_id: string;
  name: string;
  detail: ProgramVersionDetail;
};

export type AppliedProgramRef = {
  program_id: string;
  name: string;
  version: number;
};

export type EdgeSummary = {
  edge_id: string;
  edge_name: string;
  sync_status: "synced" | "out_of_sync" | "not_synced" | string;
  last_seen_unix_ms: number | null;
  pending_commands: number;
  recent_results: number;
  applied_program: AppliedProgramRef | null;
  last_poll_unix_ms: number | null;
  last_result_unix_ms: number | null;
  total_polls: number;
  total_results: number;
  last_telemetry: TelemetrySnapshot | null;
};

export type EdgeListResponse = { edges: EdgeSummary[] };

export type TelemetrySnapshot = {
  uptime_seconds: number;
  program_loaded: boolean;
  debug_session_active: boolean;
  debug_session_attached: boolean;
  debug_session_current_line: number | null;
  debug_session_request_id: string | null;
  data_requests_total: number;
  vm_execution_errors_total: number;
  program_apply_success_total: number;
  program_apply_failure_total: number;
  control_rpc_polls_success_total: number;
  control_rpc_polls_error_total: number;
  control_rpc_results_success_total: number;
  control_rpc_results_error_total: number;
};

export type EdgeTrafficPoint = {
  unix_ms: number;
  requests: number;
  status_2xx: number;
  status_3xx: number;
  status_4xx: number;
  status_5xx: number;
  latency_p50_ms: number;
  latency_p90_ms: number;
  latency_p99_ms: number;
  upstream_latency_p50_ms: number;
  upstream_latency_p90_ms: number;
  upstream_latency_p99_ms: number;
  edge_latency_p50_ms: number;
  edge_latency_p90_ms: number;
  edge_latency_p99_ms: number;
};

export type EdgeDetailResponse = {
  summary: EdgeSummary;
  pending_command_types: string[];
  traffic_series: EdgeTrafficPoint[];
};

export type QueueResponse = {
  command_id: string;
  pending_commands: number;
};

export type DebugSessionPhase =
  | "queued"
  | "waiting_for_start_result"
  | "waiting_for_recordings"
  | "waiting_for_attach"
  | "attached"
  | "replay_ready"
  | "stopped"
  | "failed";

export type DebugSessionMode = "interactive" | "recording";

export type DebugRecordingSummary = {
  recording_id: string;
  sequence: number;
  created_unix_ms: number;
  frame_count: number;
  terminal_status: string | null;
  request_id: string | null;
  request_path: string | null;
};

export type DebugSessionSummary = {
  session_id: string;
  edge_id: string;
  edge_name: string;
  phase: DebugSessionPhase;
  mode: DebugSessionMode;
  header_name: string | null;
  nonce_header_value: string | null;
  request_id: string | null;
  request_path: string | null;
  recording_target_count: number | null;
  recording_count: number;
  current_line: number | null;
  created_unix_ms: number;
  updated_unix_ms: number;
  message: string | null;
};

export type DebugSessionDetail = {
  session_id: string;
  edge_id: string;
  edge_name: string;
  phase: DebugSessionPhase;
  mode: DebugSessionMode;
  header_name: string | null;
  nonce_header_value: string | null;
  request_id: string | null;
  tcp_addr: string;
  request_path: string | null;
  recording_target_count: number | null;
  recordings: DebugRecordingSummary[];
  selected_recording_id: string | null;
  start_command_id: string;
  stop_command_id: string | null;
  current_line: number | null;
  source_flavor: string | null;
  source_code: string | null;
  breakpoints: number[];
  created_unix_ms: number;
  updated_unix_ms: number;
  attached_unix_ms: number | null;
  message: string | null;
  last_output: string | null;
};

export type DebugSessionListResponse = {
  sessions: DebugSessionSummary[];
};

export type DebugSessionsStreamSnapshot = {
  kind: "snapshot";
  sessions: DebugSessionSummary[];
  selected_session: DebugSessionDetail | null;
};

export type DebugCommandResponse = {
  phase: DebugSessionPhase;
  output: string;
  current_line: number | null;
  attached: boolean;
};

export type DebugCommandRequest =
  | { kind: "where" }
  | { kind: "step" }
  | { kind: "next" }
  | { kind: "continue" }
  | { kind: "out" }
  | { kind: "select_recording"; recording_id: string }
  | { kind: "break_line"; line: number }
  | { kind: "clear_line"; line: number }
  | { kind: "print_var"; name: string }
  | { kind: "locals" }
  | { kind: "stack" };

export type RunDebugCommandOptions = {
  silent?: boolean;
  refresh?: boolean;
};

export type RunDebugCommandFn = (
  request: DebugCommandRequest,
  options?: RunDebugCommandOptions
) => Promise<DebugCommandResponse | null>;

export type FlowNodeData = {
  blockId: string;
  definition: UiBlockDefinition;
  values: Record<string, string>;
  connectedInputs: Record<string, boolean>;
  onValueChange: (nodeId: string, key: string, value: string) => void;
  onDelete: (nodeId: string) => void;
};

export type FlowNode = Node<FlowNodeData, "blockNode">;

export type FlowEdgeData = {
  source_output: string;
  target_input: string;
};

export type FlowEdge = Edge<FlowEdgeData>;
