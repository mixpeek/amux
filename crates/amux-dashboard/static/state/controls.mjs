export function actionFromHandler(source, registered = {}) {
  // Ignore member calls such as event.stopPropagation(), but preserve the
  // actual command even when it follows event housekeeping or an if guard.
  const calls = [...String(source || '').matchAll(/(?:^|[^\w.$])([\w$]+)\s*\(/g)]
    .map(match => match[1]).filter(name => !['if','for','while','switch','function'].includes(name));
  return calls.find(name => registered[name]) || calls[0];
}
