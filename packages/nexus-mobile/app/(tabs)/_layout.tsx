/**
 * _layout.tsx - Tab-based root layout after auth
 */
import React, { useEffect } from "react";
import { Tabs } from "expo-router";
import { View, Text, StyleSheet } from "react-native";
import { store, useStore } from "../lib/store";
import { gateway } from "../lib/gateway";

function UnreadBadge({ count }: { count: number }) {
  if (!count) return null;
  return (
    <View style={styles.badge}>
      <Text style={styles.badgeText}>{count > 99 ? "99+" : count}</Text>
    </View>
  );
}

export default function TabLayout() {
  useStore();

  useEffect(() => {
    if (!store.isAuthenticated) return;
    // Load servers/DMs/relationships on every mount (covers app restore from
    // storage where loadInitialData was never called after hydration).
    store.loadInitialData().catch(console.error);
    gateway.connect().catch(console.error);
    return () => gateway.disconnect();
  }, []);

  const unreadCount = Object.keys(store.unreadChannels).length;

  return (
    <Tabs
      screenOptions={{
        headerStyle: { backgroundColor: "#071327" },
        headerTintColor: "#d6f4ff",
        tabBarStyle: { backgroundColor: "#071327", borderTopColor: "#0f2040" },
        tabBarActiveTintColor: "#27c9a5",
        tabBarInactiveTintColor: "#7890b0",
      }}
    >
      <Tabs.Screen
        name="index"
        options={{
          title: "Servers",
          headerShown: false,
          tabBarIcon: ({ color }) => <Text style={{ fontSize: 20, color }}>#</Text>,
        }}
      />
      <Tabs.Screen
        name="dms"
        options={{
          title: "Messages",
          tabBarIcon: ({ color }) => (
            <View>
              <Text style={{ fontSize: 20, color }}>@</Text>
              {unreadCount > 0 && <UnreadBadge count={unreadCount} />}
            </View>
          ),
        }}
      />
      <Tabs.Screen
        name="search"
        options={{
          title: "Search",
          tabBarIcon: ({ color }) => <Text style={{ fontSize: 20, color }}>⌕</Text>,
        }}
      />
      <Tabs.Screen
        name="settings"
        options={{
          title: "Settings",
          tabBarIcon: ({ color }) => <Text style={{ fontSize: 20, color }}>⚙</Text>,
        }}
      />
      <Tabs.Screen
        name="profile"
        options={{
          title: "Profile",
          tabBarIcon: ({ color }) => <Text style={{ fontSize: 20, color }}>P</Text>,
        }}
      />
    </Tabs>
  );
}

const styles = StyleSheet.create({
  badge: {
    position: "absolute", top: -4, right: -8,
    backgroundColor: "#ed4245", borderRadius: 10,
    minWidth: 16, height: 16, justifyContent: "center", alignItems: "center",
    paddingHorizontal: 4,
  },
  badgeText: { color: "#fff", fontSize: 10, fontWeight: "700" },
});
