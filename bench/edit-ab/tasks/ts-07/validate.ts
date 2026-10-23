// Field validators that return a list of problems, empty when the value is fine.

export type Problems = string[];

export interface PasswordRules {
  minLength?: number;
  requireDigit?: boolean;
  requireUpper?: boolean;
}

export function validateEmail(value: string): Problems {
  const problems: Problems = [];
  const text = value.trim();
  if (text.length > 254) {
    problems.push("email is too long");
  }
  const at = text.indexOf("@");
  if (at < 1 || at !== text.lastIndexOf("@")) {
    problems.push("email needs exactly one @ after a name");
    return problems;
  }
  const domain = text.slice(at + 1);
  if (!domain.includes(".") || domain.startsWith(".") || domain.endsWith(".")) {
    problems.push("email domain is not valid");
  }
  return problems;
}

export function validateAge(value: number): Problems {
  const problems: Problems = [];
  if (!Number.isInteger(value)) {
    problems.push("age must be a whole number");
  }
  if (value < 0 && value > 150) {
    problems.push("age must be between 0 and 150");
  }
  return problems;
}

export function validatePassword(value: string, rules: PasswordRules = {}): Problems {
  const minLength = rules.minLength ?? 6;
  const requireDigit = rules.requireDigit ?? true;
  const requireUpper = rules.requireUpper ?? true;
  const problems: Problems = [];
  if (value.length < minLength) {
    problems.push(`password needs at least ${minLength} characters`);
  }
  if (requireDigit && !/[0-9]/.test(value)) {
    problems.push("password needs a digit");
  }
  if (requireUpper && !/[A-Z]/.test(value)) {
    problems.push("password needs an upper case letter");
  }
  return problems;
}

export function validateUsername(value: string): Problems {
  const problems: Problems = [];
  if (value.length < 3 || value.length > 20) {
    problems.push("username must have 3 to 20 characters");
  }
  if (!/^[a-z0-9_]*$/.test(value)) {
    problems.push("username may only use a-z, 0-9 and _");
  }
  return problems;
}

export interface Signup {
  email: string;
  age: number;
  password: string;
  username: string;
}

// Validate a whole form and return the problems per field, leaving out the
// fields that are fine.
export function validateSignup(form: Signup): Record<string, Problems> {
  const all: Record<string, Problems> = {
    email: validateEmail(form.email),
    age: validateAge(form.age),
    password: validatePassword(form.password),
    username: validateUsername(form.username),
  };
  const out: Record<string, Problems> = {};
  for (const [field, problems] of Object.entries(all)) {
    if (problems.length > 0) {
      out[field] = problems;
    }
  }
  return out;
}

export function isValid(form: Signup): boolean {
  return Object.keys(validateSignup(form)).length === 0;
}

export function firstProblem(form: Signup): string | null {
  for (const problems of Object.values(validateSignup(form))) {
    return problems[0];
  }
  return null;
}
