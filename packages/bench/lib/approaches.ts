export type Approach = 'json-parse' | 'bote';

export const APPROACHES: Approach[] = ['json-parse', 'bote'];

export const APPROACH_LABEL: Record<Approach, string> = {
  'json-parse': 'JSON.parse',
  bote: 'bote',
};
