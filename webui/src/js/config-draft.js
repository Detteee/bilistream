// A form owns the values it loaded; background dashboard refreshes do not
// change that baseline. Submit only edited fields and their original values.
export function createConfigPatch(current, baseline) {
  if (!baseline) throw new Error('请先成功加载配置后再保存');
  const patch = {};
  const expected = {};
  for (const [key, value] of Object.entries(current)) {
    if (JSON.stringify(value) !== JSON.stringify(baseline[key])) {
      patch[key] = value;
      expected[key] = baseline[key];
    }
  }
  return Object.keys(patch).length ? { ...patch, expected } : null;
}
