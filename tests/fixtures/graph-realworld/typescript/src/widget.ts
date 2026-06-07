export type WidgetProps = {
  title: string;
};

export default function DefaultWidget(props: WidgetProps) {
  return props.title;
}

export function renderWidget() {
  return DefaultWidget({ title: "ok" });
}
