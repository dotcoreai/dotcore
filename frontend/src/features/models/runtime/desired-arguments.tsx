import type { ArgumentToken } from "./desired-argv";

/** A bordered listing of the desired argv tokens. */
export function DesiredArgumentList({
  argv,
  className,
}: {
  argv: readonly ArgumentToken[];
  className: string;
}) {
  return (
    <div className={`${className} rounded-md border border-border p-3`}>
      <ArgumentTokenList ariaLabel="Desired argument tokens" tokens={argv} />
    </div>
  );
}

export function ArgumentTokenList({
  ariaLabel,
  tokens,
}: {
  ariaLabel: string;
  tokens: readonly ArgumentToken[];
}) {
  return (
    <ol aria-label={ariaLabel} className="grid list-decimal gap-1 pl-5 text-xs leading-relaxed">
      {tokens.map((token, index) => (
        <li className="min-w-0 pl-1" key={index}>
          {token.placeholder ? (
            <code className="whitespace-pre-wrap break-all text-primary">{token.text}</code>
          ) : (
            <code className="whitespace-pre-wrap break-all">{JSON.stringify(token.text)}</code>
          )}
        </li>
      ))}
    </ol>
  );
}
