import { appendFileSync } from 'node:fs';

// Observe native lifecycle and built payloads; never return a replacement or log headers.
export default function (pi) {
  const auditFile = process.env.LUNA_AUDIT_FILE;
  const bytes = (value) => Buffer.byteLength(typeof value === 'string' ? value : JSON.stringify(value ?? null));
  const jsonBytes = (value) => Buffer.byteLength(JSON.stringify(value ?? null));
  // The observer must never break the measured arm: missing env or I/O errors are swallowed.
  const log = (record) => {
    if (!auditFile) return;
    try { appendFileSync(auditFile, JSON.stringify({ time_ms: Date.now(), ...record }) + '\n'); } catch {}
  };
  pi.on('before_provider_request', (event) => {
    try {
      const p = event.payload;
      // Same disjoint item accounting as slim-core provider_request_components.
      let systemBytes = p.instructions !== undefined ? jsonBytes(p.instructions) : p.system !== undefined ? jsonBytes(p.system) : 0;
      let historyBytes = 0;
      let resultBytes = 0;
      for (const field of ['messages', 'input']) {
        if (p[field] === undefined) continue;
        if (!Array.isArray(p[field])) { historyBytes += jsonBytes(p[field]); continue; }
        for (const item of p[field]) {
          if (['system', 'developer'].includes(item.role)) systemBytes += jsonBytes(item);
          else if (['function_call_output', 'tool_result'].includes(item.type) || item.role === 'tool' ||
                   (Array.isArray(item.content) && item.content.some(c => ['function_call_output', 'tool_result'].includes(c?.type)))) resultBytes += jsonBytes(item);
          else historyBytes += jsonBytes(item);
        }
      }
      log({ type: 'request', model: p.model, reasoning: p.reasoning ?? (p.reasoning_effort ? {effort: p.reasoning_effort} : null), thinking: p.thinking, service_tier: p.service_tier ?? null,
        component_schema: 'slim-components-v1',
        payload_bytes: bytes(p), system_bytes: systemBytes, tool_schema_bytes: jsonBytes(p.tools ?? []),
        history_bytes: historyBytes, tool_names: (p.tools ?? []).map(t => t.name ?? t.function?.name),
        tool_result_bytes: resultBytes,
        payload: JSON.parse(JSON.stringify(p, (key, value) => key === 'encrypted_content' ? '[omitted]' : value)) });
    } catch {}
  });
  pi.on('after_provider_response', (event) => { try { log({ type: 'response_headers', status: event.status }); } catch {} });
  for (const type of ['session_start', 'agent_start', 'agent_end', 'message_end', 'tool_execution_start', 'tool_execution_end']) {
    pi.on(type, (event) => { try { log(JSON.parse(JSON.stringify(event, (key, value) =>
      ['thinkingSignature', 'textSignature'].includes(key) ? '[omitted]' : value))); } catch {} });
  }
}
