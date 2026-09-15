import { appendFileSync } from 'node:fs';

// Observe native lifecycle and built payloads; never return a replacement or log headers.
export default function (pi) {
  const bytes = (value) => Buffer.byteLength(typeof value === 'string' ? value : JSON.stringify(value ?? null));
  const log = (record) => appendFileSync(process.env.LUNA_AUDIT_FILE!, JSON.stringify({ time_ms: Date.now(), ...record }) + '\n');
  pi.on('before_provider_request', (event) => {
    const p = event.payload;
    const history = p.input ?? (p.messages ?? []).filter(m => !['system', 'developer'].includes(m.role));
    const system = p.instructions ?? p.system ?? (p.messages ?? []).filter(m => ['system', 'developer'].includes(m.role));
    log({ type: 'request', model: p.model, reasoning: p.reasoning ?? (p.reasoning_effort ? {effort: p.reasoning_effort} : null), thinking: p.thinking, service_tier: p.service_tier ?? null,
      payload_bytes: bytes(p), system_bytes: bytes(system), tool_schema_bytes: bytes(p.tools ?? []),
      history_bytes: bytes(history), tool_names: (p.tools ?? []).map(t => t.name ?? t.function?.name),
      tool_result_bytes: history.filter(i => i.type === 'function_call_output' || i.role === 'tool').reduce((n, i) => n + bytes(i.output ?? i.content), 0),
      payload: JSON.parse(JSON.stringify(p, (key, value) => key === 'encrypted_content' ? '[omitted]' : value)) });
  });
  pi.on('after_provider_response', (event) => { log({ type: 'response_headers', status: event.status }); });
  for (const type of ['session_start', 'agent_start', 'agent_end', 'message_end', 'tool_execution_start', 'tool_execution_end']) {
    pi.on(type, (event) => { log(JSON.parse(JSON.stringify(event, (key, value) =>
      ['thinkingSignature', 'textSignature'].includes(key) ? '[omitted]' : value))); });
  }
}
