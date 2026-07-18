// Normalise the Tauri model-catalog response once so forms can cheaply look up suggestions.

export function indexModelCatalogs(catalogs) {
  const indexed = {};
  for (const catalog of Array.isArray(catalogs) ? catalogs : []) {
    if (!catalog || typeof catalog.backend !== "string" || !catalog.backend) continue;
    const seen = new Set();
    const models = [];
    for (const model of Array.isArray(catalog.models) ? catalog.models : []) {
      const value = typeof model?.value === "string" ? model.value.trim() : "";
      if (!value || seen.has(value)) continue;
      seen.add(value);
      models.push({ value, label: typeof model.label === "string" && model.label ? model.label : value });
    }
    indexed[catalog.backend] = { ...catalog, models };
  }
  return indexed;
}

export function primaryRoleCatalog(indexed, backends) {
  const primary = Array.isArray(backends) ? backends[0] : null;
  return primary ? indexed?.[primary] || null : null;
}
