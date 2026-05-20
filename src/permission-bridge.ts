import type {
  PermissionOption,
  PermissionOptionKind,
  RequestPermissionRequest,
  RequestPermissionResponse,
} from "@agentclientprotocol/sdk";

export interface PermissionPolicy {
  readonly autoApprove: ReadonlySet<PermissionOptionKind>;
}

const pickOption = (
  options: ReadonlyArray<PermissionOption>,
  kinds: ReadonlyArray<PermissionOptionKind>,
): PermissionOption | undefined => {
  for (const kind of kinds) {
    const match = options.find((opt) => opt.kind === kind);
    if (match) return match;
  }
  return undefined;
};

export const handleRequestPermission = async (
  params: RequestPermissionRequest,
  policy: PermissionPolicy,
): Promise<RequestPermissionResponse> => {
  const allowOption = pickOption(params.options, [
    "allow_once",
    "allow_always",
  ]);
  const rejectOption = pickOption(params.options, [
    "reject_once",
    "reject_always",
  ]);

  if (allowOption && policy.autoApprove.has(allowOption.kind)) {
    return {
      outcome: { outcome: "selected", optionId: allowOption.optionId },
    };
  }

  const fallback = allowOption ?? rejectOption ?? params.options[0];
  if (!fallback) {
    return { outcome: { outcome: "cancelled" } };
  }
  return {
    outcome: { outcome: "selected", optionId: fallback.optionId },
  };
};

export const defaultPolicy: PermissionPolicy = {
  autoApprove: new Set<PermissionOptionKind>(["allow_once"]),
};
