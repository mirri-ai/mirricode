// Raw-string imports for prompt sources. Required so tsgo can resolve
// ?raw imports when following @mirri-ai/agent-core re-exports.

declare module '*?raw' {
  const content: string;
  export default content;
}
