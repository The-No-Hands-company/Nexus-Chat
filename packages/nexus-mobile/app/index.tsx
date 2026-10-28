/**
 * app/index.tsx - Root redirect (auth guard handles routing)
 * This file exists for the Expo Router entry point; actual routing
 * is handled by app/_layout.tsx which redirects based on auth state.
 */
import { Redirect } from "expo-router";
import { store, useStore } from "./lib/store";

export default function Index() {
  useStore();
  if (store.isAuthenticated) {
    return <Redirect href="/(tabs)" />;
  }
  return <Redirect href="/(auth)/login" />;
}
