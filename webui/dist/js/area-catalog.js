// Shared official-area picker for setup and configuration management.
import { getJson } from './api.js';

let catalogRequest = null;
export function loadAreaCatalog() {
  if (!catalogRequest) {
    catalogRequest = getJson('/api/areas/catalog').then(result => {
      if (!result.success || !Array.isArray(result.data)) throw new Error(result.message || '无法加载官方分区');
      return result.data;
    }).catch(error => { catalogRequest = null; throw error; });
  }
  return catalogRequest;
}
export function fillAreaCatalog(select, areas, placeholder = '选择官方分区...') {
  const old = select.value;
  const option = document.createElement('option'); option.value = ''; option.textContent = placeholder;
  select.replaceChildren(option);
  const groups = new Map();
  for (const area of areas) {
    const parent = area.parent_name || '本地分区';
    if (!groups.has(parent)) {
      const group = document.createElement('optgroup'); group.label = parent;
      groups.set(parent, group); select.appendChild(group);
    }
    const item = document.createElement('option'); item.value = String(area.id); item.textContent = `${area.name} (${area.id})`;
    groups.get(parent).appendChild(item);
  }
  select.value = areas.some(area => String(area.id) === old) ? old : areas.some(area => area.id === 235) ? '235' : '';
}
