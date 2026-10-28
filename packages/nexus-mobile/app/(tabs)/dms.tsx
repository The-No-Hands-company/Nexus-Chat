/**
 * (tabs)/dms.tsx - Direct Messages list
 */
import React, { useState, useEffect, useRef } from "react";
import {
  View, Text, FlatList, Pressable, StyleSheet,
  SafeAreaView, TextInput, Modal, Alert, ActivityIndicator,
} from "react-native";
import { useRouter } from "expo-router";
import { store, useStore } from "../lib/store";
import type { User, DmChannel } from "../lib/api";

function formatTime(iso: string): string {
  const d = new Date(iso);
  const diff = Date.now() - d.getTime();
  if (diff < 60_000) return "now";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}h`;
  const days = Math.floor(diff / 86_400_000);
  if (days < 7) return d.toLocaleDateString(undefined, { weekday: "short" });
  return d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

function getDmName(dm: DmChannel, myId?: string): string {
  if (dm.name) return dm.name;
  const others = dm.recipients.filter(r => r.id !== myId);
  if (others.length === 1) return others[0].displayName ?? others[0].username;
  if (others.length > 1) return others.map(r => r.username).join(", ");
  return dm.recipients[0]?.username ?? "Direct Message";
}

export default function DMsScreen() {
  useStore();
  const router = useRouter();
  const [showSearch, setShowSearch] = useState(false);
  const [searchQuery, setSearchQuery] = useState("");
  const [searchResults, setSearchResults] = useState<User[]>([]);
  const [searching, setSearching] = useState(false);
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Debounced auto-search
  useEffect(() => {
    if (!showSearch) return;
    if (!searchQuery.trim()) {
      setSearchResults([]);
      return;
    }
    if (debounceRef.current) clearTimeout(debounceRef.current);
    debounceRef.current = setTimeout(async () => {
      setSearching(true);
      try {
        const results = await store.api.searchUsers(searchQuery.trim());
        setSearchResults(results);
      } catch (e: any) {
        Alert.alert("Error", e.message);
      } finally {
        setSearching(false);
      }
    }, 300);
    return () => { if (debounceRef.current) clearTimeout(debounceRef.current); };
  }, [searchQuery, showSearch]);

  function closeSearch() {
    setShowSearch(false);
    setSearchQuery("");
    setSearchResults([]);
  }

  async function handleOpenDM(userId: string) {
    closeSearch();
    try {
      const dm = await store.createDM(userId);
      await store.selectDM(dm.id);
      router.push(`/channel/${dm.id}`);
    } catch (e: any) {
      Alert.alert("Error", e.message);
    }
  }

  // Sort DMs by last loaded message time, then by id fallback
  const myId = store.session?.user.id;
  const sortedDms = [...store.dmChannels].sort((a, b) => {
    const amsgs = store.messages[a.id];
    const bmsgs = store.messages[b.id];
    const at = amsgs?.length ? amsgs[amsgs.length - 1].createdAt : "";
    const bt = bmsgs?.length ? bmsgs[bmsgs.length - 1].createdAt : "";
    if (!at && !bt) return 0;
    if (!at) return 1;
    if (!bt) return -1;
    return new Date(bt).getTime() - new Date(at).getTime();
  });

  const renderDM = ({ item }: { item: DmChannel }) => {
    const name = getDmName(item, myId);
    const msgs = store.messages[item.id];
    const last = msgs?.length ? msgs[msgs.length - 1] : null;
    const isUnread = !!store.unreadChannels[item.id];

    return (
      <Pressable
        onPress={async () => { await store.selectDM(item.id); router.push(`/channel/${item.id}`); }}
        style={[styles.dmRow, isUnread && styles.dmRowUnread]}
      >
        <View style={styles.avatarWrap}>
          <View style={styles.avatar}>
            <Text style={styles.avatarText}>{name[0]?.toUpperCase()}</Text>
          </View>
          {isUnread && <View style={styles.unreadDot} />}
        </View>
        <View style={styles.dmInfo}>
          <View style={styles.dmTopRow}>
            <Text style={[styles.dmName, isUnread && styles.dmNameUnread]} numberOfLines={1}>{name}</Text>
            {last && <Text style={styles.timestamp}>{formatTime(last.createdAt)}</Text>}
          </View>
          {last ? (
            <Text style={[styles.dmPreview, isUnread && styles.dmPreviewUnread]} numberOfLines={1}>
              {last.authorId === myId ? "You: " : ""}{last.content || "📎 Attachment"}
            </Text>
          ) : (
            <Text style={styles.dmMeta}>{item.channelType === "group_dm" ? "Group conversation" : "Tap to open"}</Text>
          )}
        </View>
      </Pressable>
    );
  };

  const renderSearchResult = ({ item }: { item: User }) => (
    <Pressable onPress={() => handleOpenDM(item.id)} style={styles.dmRow}>
      <View style={styles.avatarWrap}>
        <View style={styles.avatar}>
          <Text style={styles.avatarText}>{item.username[0]?.toUpperCase()}</Text>
        </View>
      </View>
      <View style={styles.dmInfo}>
        <Text style={styles.dmName}>{item.displayName ?? item.username}</Text>
        <Text style={styles.dmMeta}>@{item.username}</Text>
      </View>
    </Pressable>
  );

  return (
    <SafeAreaView style={styles.container}>
      <View style={styles.header}>
        <Text style={styles.headerTitle}>Messages</Text>
        <Pressable onPress={() => setShowSearch(true)} style={styles.headerBtn}>
          <Text style={styles.headerBtnText}>+ New</Text>
        </Pressable>
      </View>

      <FlatList
        data={sortedDms}
        keyExtractor={item => item.id}
        renderItem={renderDM}
        contentContainerStyle={styles.list}
        ListEmptyComponent={
          <View style={styles.empty}>
            <Text style={styles.emptyText}>No conversations yet</Text>
            <Text style={styles.emptySubtext}>Tap "+ New" to start a DM</Text>
          </View>
        }
      />

      <Modal visible={showSearch} animationType="slide" presentationStyle="pageSheet">
        <SafeAreaView style={styles.modalContainer}>
          <View style={styles.modalHeader}>
            <Text style={styles.modalTitle}>New Message</Text>
            <Pressable onPress={closeSearch}>
              <Text style={styles.closeBtn}>Cancel</Text>
            </Pressable>
          </View>
          <View style={styles.searchRow}>
            <TextInput
              style={styles.searchInput}
              placeholder="Search users..."
              placeholderTextColor="#7890b0"
              value={searchQuery}
              onChangeText={setSearchQuery}
              autoCapitalize="none"
              autoCorrect={false}
              autoFocus
            />
            {searching && <ActivityIndicator style={styles.searchSpinner} color="#27c9a5" />}
          </View>
          <FlatList
            data={searchResults}
            keyExtractor={item => item.id}
            renderItem={renderSearchResult}
            contentContainerStyle={styles.list}
            ListEmptyComponent={
              searchQuery.length > 0 && !searching ? (
                <View style={styles.empty}>
                  <Text style={styles.emptyText}>No users found</Text>
                </View>
              ) : null
            }
          />
        </SafeAreaView>
      </Modal>
    </SafeAreaView>
  );
}

const styles = StyleSheet.create({
  container: { flex: 1, backgroundColor: "#030a14" },
  header: { flexDirection: "row", alignItems: "center", padding: 12, backgroundColor: "#071327", borderBottomWidth: 1, borderBottomColor: "#0f2040" },
  headerTitle: { flex: 1, fontSize: 18, fontWeight: "700", color: "#d6f4ff" },
  headerBtn: { paddingHorizontal: 12, paddingVertical: 6 },
  headerBtnText: { color: "#27c9a5", fontWeight: "700", fontSize: 15 },
  list: { padding: 8 },
  dmRow: { flexDirection: "row", alignItems: "center", padding: 12, borderRadius: 10, marginBottom: 4 },
  dmRowUnread: { backgroundColor: "#0b1a30" },
  avatarWrap: { position: "relative", marginRight: 12 },
  avatar: { width: 44, height: 44, borderRadius: 22, backgroundColor: "#112544", justifyContent: "center", alignItems: "center" },
  avatarText: { color: "#27c9a5", fontSize: 18, fontWeight: "700" },
  unreadDot: { position: "absolute", bottom: 0, right: 0, width: 12, height: 12, borderRadius: 6, backgroundColor: "#ed4245", borderWidth: 2, borderColor: "#030a14" },
  dmInfo: { flex: 1, minWidth: 0 },
  dmTopRow: { flexDirection: "row", alignItems: "center", justifyContent: "space-between", marginBottom: 2 },
  dmName: { flex: 1, color: "#d6f4ff", fontSize: 15, fontWeight: "600" },
  dmNameUnread: { color: "#fff", fontWeight: "700" },
  dmPreview: { color: "#7890b0", fontSize: 13 },
  dmPreviewUnread: { color: "#9ec5d5" },
  dmMeta: { color: "#7890b0", fontSize: 12 },
  timestamp: { color: "#7890b0", fontSize: 11, marginLeft: 6, flexShrink: 0 },
  empty: { alignItems: "center", paddingTop: 60 },
  emptyText: { color: "#9ec5d5", fontSize: 16 },
  emptySubtext: { color: "#7890b0", fontSize: 14, marginTop: 8 },
  modalContainer: { flex: 1, backgroundColor: "#030a14" },
  modalHeader: { flexDirection: "row", alignItems: "center", padding: 12, backgroundColor: "#071327", borderBottomWidth: 1, borderBottomColor: "#0f2040" },
  modalTitle: { flex: 1, fontSize: 18, fontWeight: "700", color: "#d6f4ff" },
  closeBtn: { color: "#27c9a5", fontSize: 16, fontWeight: "600" },
  searchRow: { padding: 12, flexDirection: "row", alignItems: "center" },
  searchInput: { flex: 1, backgroundColor: "#112544", color: "#d6f4ff", borderRadius: 10, padding: 12, fontSize: 16, borderWidth: 1, borderColor: "#1e3a5f" },
  searchSpinner: { marginLeft: 10 },
});
