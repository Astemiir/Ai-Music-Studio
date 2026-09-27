/**
 * The official MiniMax Music 3 demo prompts that ship with the engine's
 * reference client, plus the split between the three caption sections.
 *
 * They are the clearest statement of what this model expects: a caption written
 * as a labelled document — Global Metadata, Vocal Details, Arrangement — and
 * lyrics carrying bracketed section tags. Loading one shows that shape straight
 * away instead of leaving a one-line prompt to guess from.
 */

const modules = import.meta.glob('../examples/music3/*.json', { eager: true }) as Record<string, { default?: unknown }>;

export interface Music3Example {
  name: string;
  caption: string;
  globalMetadata: string;
  vocalDetails: string;
  arrangement: string;
  lyrics: string;
  duration: number;
}

const SECTIONS = ['Global Metadata', 'Vocal Details', 'Arrangement'] as const;

/**
 * Splits a caption on its section headings. A caption that does not carry them
 * — a hand-written one, for instance — stays whole in the first field so
 * nothing is silently dropped.
 */
export function splitCaption(caption: string): { globalMetadata: string; vocalDetails: string; arrangement: string } {
  const positions = SECTIONS.map(section => ({ section, index: caption.indexOf(section) }));
  if (positions.some(entry => entry.index < 0)) {
    return { globalMetadata: caption.trim(), vocalDetails: '', arrangement: '' };
  }
  const slice = (from: number, to: number, heading: string) =>
    caption.slice(from, to).replace(heading, '').trim();
  return {
    globalMetadata: slice(positions[0].index, positions[1].index, SECTIONS[0]),
    vocalDetails: slice(positions[1].index, positions[2].index, SECTIONS[1]),
    arrangement: slice(positions[2].index, caption.length, SECTIONS[2]),
  };
}

/** Rebuilds the single caption string the engine takes from the three panes. */
export function joinCaption(globalMetadata: string, vocalDetails: string, arrangement: string): string {
  return SECTIONS.map((heading, index) => {
    // a trigger's comma with nothing after it is not part of the caption
    const body = [globalMetadata, vocalDetails, arrangement][index].trim().replace(/,$/, '').trimEnd();
    return body ? `${heading}\n${body}` : '';
  })
    .filter(Boolean)
    .join('\n');
}

const examples: Music3Example[] = Object.entries(modules).map(([path, module]) => {
  const data = (module.default ?? module) as { caption?: string; lyrics?: string; duration?: number };
  const caption = String(data.caption ?? '');
  return {
    name: path.split('/').pop()?.replace('.json', '') ?? 'example',
    caption,
    ...splitCaption(caption),
    lyrics: String(data.lyrics ?? ''),
    duration: Number(data.duration ?? 60),
  };
});

export function randomExample(): Music3Example {
  return examples[Math.floor(Math.random() * examples.length)];
}

export const exampleCount = examples.length;

/**
 * A caption for this model is a labelled document, so showing it raw in a list
 * subtitle reads as "Global Metadata Basic Attributes: bpm is 118. key is ...".
 * This keeps the description and drops the scaffolding.
 */
export function captionSummary(caption: string): string {
  const skip = /^(global metadata|vocal details|arrangement)/i;
  const technical = /(bpm|key|scale|tempo|time signature) is/i;
  return caption
    .split(/[\n.]/)
    .map(part => part.trim())
    .filter(part => part.length > 0 && !skip.test(part) && !technical.test(part))
    .join('. ')
    .trim();
}
