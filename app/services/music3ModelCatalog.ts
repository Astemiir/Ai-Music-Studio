export type Music3Component = {
  id: string;
  kind: string;
  filename: string;
  bytes: number;
  sha256: string;
};

export const MUSIC3_COMPONENT_KINDS = ['lm', 'depth', 'condition', 'dit', 'vocoder'] as const;

const labels: Record<(typeof MUSIC3_COMPONENT_KINDS)[number], string> = {
  lm: 'Language model',
  depth: 'Depth decoder',
  condition: 'Condition encoder',
  dit: 'DiT',
  vocoder: 'Vocoder',
};

export const componentKindLabel = (kind: string) => labels[kind as keyof typeof labels] || kind;

export const componentPrecision = (component: Music3Component) => {
  const matched = component.filename.match(/-(BF16|F32|Q\d+(?:_K(?:_M)?|_0)?)\.gguf$/i);
  return matched?.[1]?.toUpperCase() || component.id;
};

export const componentsByKind = (components: Music3Component[]) =>
  MUSIC3_COMPONENT_KINDS.map((kind) => ({ kind, components: components.filter((component) => component.kind === kind) }));

export const completeCustomComponentIds = (components: Music3Component[], selectedByKind: Record<string, string>) => {
  const ids = MUSIC3_COMPONENT_KINDS.map((kind) => selectedByKind[kind]).filter((id): id is string => Boolean(id));
  if (ids.length !== MUSIC3_COMPONENT_KINDS.length || new Set(ids).size !== ids.length) return null;
  const selected = ids.map((id) => components.find((component) => component.id === id));
  if (selected.some((component) => !component)) return null;
  if (new Set(selected.map((component) => component!.kind)).size !== MUSIC3_COMPONENT_KINDS.length) return null;
  return ids;
};

export const selectedComponentBytes = (components: Music3Component[], ids: string[] | null) =>
  ids?.reduce((total, id) => total + (components.find((component) => component.id === id)?.bytes || 0), 0) || 0;
