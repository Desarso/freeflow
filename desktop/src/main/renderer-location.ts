export function developmentRendererUrl(
  isPackaged: boolean,
  environment: NodeJS.ProcessEnv = process.env
): string | null {
  if (isPackaged) return null;
  const url = environment.ELECTRON_RENDERER_URL?.trim();
  return url || null;
}
