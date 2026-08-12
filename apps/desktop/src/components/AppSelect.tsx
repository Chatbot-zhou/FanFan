import { Select, type SelectProps } from "antd";
import type { ReactNode } from "react";

export interface AppSelectOption<Value extends string = string> {
  value: Value;
  label: ReactNode;
  disabled?: boolean;
}

interface AppSelectProps<Value extends string = string> {
  value: Value;
  options: AppSelectOption<Value>[];
  onChange: (value: Value) => void;
  ariaLabel: string;
  disabled?: boolean;
  placeholder?: string;
  showSearch?: boolean;
  className?: string;
  labelRender?: SelectProps<Value>["labelRender"];
}

export function AppSelect<Value extends string = string>({
  value,
  options,
  onChange,
  ariaLabel,
  disabled = false,
  placeholder,
  showSearch = false,
  className,
  labelRender,
}: AppSelectProps<Value>) {
  return (
    <Select<Value>
      aria-label={ariaLabel}
      className={["app-select", className].filter(Boolean).join(" ")}
      classNames={{ popup: { root: "app-select-popup" } }}
      value={value}
      options={options as SelectProps<Value>["options"]}
      onChange={onChange}
      disabled={disabled}
      placeholder={placeholder}
      showSearch={showSearch}
      optionFilterProp="label"
      popupMatchSelectWidth
      labelRender={labelRender}
    />
  );
}
