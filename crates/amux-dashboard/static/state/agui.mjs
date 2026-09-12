// Interoperability projection only. Commands do not masquerade as chat runs.
export function projectEvent(event) {
  const value = event.payload || {};
  switch (event.kind) {
    case 'run.started':
      if (!value.threadId || !value.runId) break;
      return {type:'RUN_STARTED', threadId:value.threadId, runId:value.runId};
    case 'message.delta':
      if (!value.messageId || typeof value.delta !== 'string') break;
      return {type:'TEXT_MESSAGE_CONTENT', messageId:value.messageId, delta:value.delta};
    case 'tool.started':
      if (!value.toolCallId || !value.toolCallName) break;
      return {type:'TOOL_CALL_START', toolCallId:value.toolCallId, toolCallName:value.toolCallName, ...(value.parentMessageId ? {parentMessageId:value.parentMessageId} : {})};
    case 'tool.completed':
      if (!value.messageId || !value.toolCallId || typeof value.content !== 'string') break;
      return {type:'TOOL_CALL_RESULT', messageId:value.messageId, toolCallId:value.toolCallId, content:value.content, role:'tool'};
    case 'state.changed':
      if (!Array.isArray(value.delta)) break;
      return {type:'STATE_DELTA', delta:value.delta};
    default: break;
  }
  return {type:'CUSTOM', name:event.kind, value};
}
