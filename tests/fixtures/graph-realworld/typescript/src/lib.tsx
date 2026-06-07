import React, { useMemo, type ReactNode } from "react";
import DefaultWidget from "./widget";
import * as WidgetNS from "./widget";
import type { WidgetProps } from "./widget";
export { DefaultWidget as ReExportedWidget } from "./widget";
export type { WidgetProps };

export interface ShellProps {
  children?: ReactNode;
}

export const useWidget = (props: WidgetProps) => {
  return useMemo(() => DefaultWidget(props), [props]);
};

export function Shell(props: ShellProps) {
  const view = WidgetNS.renderWidget();
  return <DefaultWidget title={view}>{props.children}</DefaultWidget>;
}
