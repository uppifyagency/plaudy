/* eslint-disable i18next/no-literal-string -- brand wordmark, never translated */
import React from "react";

// ponytail: text wordmark stands in until a designed Plaudy logotype lands (asset task);
// same width/height/className contract as the old SVG so callers don't change again.
const PlaudyTextLogo = ({
  width,
  height,
  className,
}: {
  width?: number;
  height?: number;
  className?: string;
}) => {
  return (
    <svg
      width={width}
      height={height}
      className={className}
      viewBox="0 0 200 56"
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
    >
      {/* Wordmark matches the landing: ink/paper "Plaudy" + the red full stop. */}
      <text
        x="100"
        y="40"
        textAnchor="middle"
        fill="var(--color-text, #0a0a0a)"
        style={{
          font: "700 38px 'Helvetica Neue', Helvetica, -apple-system, system-ui, sans-serif",
          letterSpacing: "-0.02em",
        }}
      >
        Plaudy
        <tspan fill="var(--color-accent, #e8340c)">.</tspan>
      </text>
    </svg>
  );
};

export default PlaudyTextLogo;
