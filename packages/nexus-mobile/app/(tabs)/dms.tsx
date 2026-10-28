/**
 * (tabs)/dms.tsx - Direct Messages list
 */
import React, { useState } from "react";
import {
  View, Text, FlatList, Pressable, StyleSheet,
  SafeAreaView, TextInput, Modal, Alert,
} from "react-native";
import { useRouter } from "expo-router";
import { store, useStore } from "../lib/store";

export default function DMsScreen() {
  useStore();
  const router = useRouter();
  const [showSearch, setShowSearch] = useState(false);
  const [searchQuery, setSearchQuery] = useState("");
  const [searchResults, setSearchResults] = useState<typeof store.relationships[0]["user"][]>([]);
  const [loading, setLoading] = useState(false);

  async function handleSearch() {
    if (!searchQuery.trim()) return;
    setLoading(true);
    try {
      const results = await store.api.searchUsers(searchQuery.trim());
      setSearchResults(results);
    } catch (e: any) {
      Alert.alert("Error", e.message);
    } finally {
      setLoading(false);
    }
  }

  async function handleOpenDM(userId: string) {
    setShowSearch(false); setSearchQuery(""); setSearchResults([]);
    try {
      const dm = await store.createDM(userId);
      await store.selectDM(dm.id);
      router.push(`/channel/${dm.id}`);
    } catch (e: any) {
      Alert.alert("Error", e.message);
    }
  }

  function getDmName(dm: typeof store.dmChannels[0]) {
    if (dm.name) return dm.name;
    if (dm.recipients.length === 1) return dm.recipients[0].displayName ?? dm.recipients[0].username;
    return dm.recipients.map(r => r.username).join(", ");
  }

  function getDmAvatar(dm: typeof store.dmChannels[0]) {
    if (dm.recipients.length === 1) return dm.recipients[0].avatar;
    return undefined;
  }

  const renderDM = ({ item }: { item: typeof store.dmChannels[0] }) => (
    <Pressable
      onPress={async () => { await store.selectDM(item.id); router.push(`/channel/${item.id}`); }}
      style={styles.dmRow}
    >
      <View style={styles.avatar}>
        <Text style={styles.avatarText}>{getDmName(item)[0]?.toUpperCase()}</Text>
      </View>
      <View style={styles.dmInfo}>
        <Text style={styles.dmName}>{getDmName(item)}</Text>
        <Text style={styles.dmMeta}>{item.channelType === "group_dm" ? "Group DM" : "DM"}</Text>
      </View>
    </Pressable>
  );

  const renderSearchResult = ({ item }: { item: typeof searchResults[0] }) => (
    <Pressable onPress={() => handleOpenDM(item.id)} style={styles.dmRow}>
      <View style={styles.avatar}><Text style={styles.avatarText}>{item.username[0]?.toUpperCase()}</Text></View>
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
          <Text style={styles.headerBtnText}>New DM</Text>
        </Pressable>
      </View>

      <FlatList
        data={store.dmChannels}
        keyExtractor={item => item.id}
        renderItem={renderDM}
        contentContainerStyle={styles.list}
        ListEmptyComponent={
          <View style={styles.empty}>
            <Text style={styles.emptyText}>No conversations yet</Text>
            <Text style={styles.emptySubtext}>Start a DM to chat privately</Text>
          </View>
        }
      />

      <Modal visible={showSearch} animationType="slide" presentationStyle="pageSheet">
        <SafeAreaView style={styles.modalContainer}>
          <View style={styles.modalHeader}>
            <Text style={styles.modalTitle}>New Message</Text>
            <Pressable onPress={() => { setShowSearch(false); setSearchQuery(""); setSearchResults([]); }}>
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
              onSubmitEditing={handleSearch}
              autoCapitalize="none"
              autoCorrect={false}
            />
          </View>
          <FlatList
            data={searchResults}
            keyExtractor={item => item.id}
            renderItem={renderSearchResult}
            contentContainerStyle={styles.list}
            ListEmptyComponent={
              searchQuery.length > 0 && !loading ? (
                <Text style={styles.emptyText}>No users found</Text>
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
  headerBtnText: { color: "#27c9a5", fontWeight: "700", fontSize: 16 },
  list: { padding: 8 },
  dmRow: { flexDirection: "row", alignItems: "center", padding: 12, borderRadius: 10, marginBottom: 4 },
  avatar: { width: 44, height: 44, borderRadius: 22, backgroundColor: "#0b1a30", justifyContent: "center", alignItems: "center", marginRight: 12 },
  avatarText: { color: "#27c9a5", fontSize: 18, fontWeight: "700" },
  dmInfo: { flex: 1 },
  dmName: { color: "#d6f4ff", fontSize: 16, fontWeight: "600" },
  dmMeta: { color: "#7890b0", fontSize: 12, marginTop: 2 },
  empty: { alignItems: "center", paddingTop: 60 },
  emptyText: { color: "#9ec5d5", fontSize: 16 },
  emptySubtext: { color: "#7890b0", fontSize: 14, marginTop: 8 },
  modalContainer: { flex: 1, backgroundColor: "#030a14" },
  modalHeader: { flexDirection: "row", alignItems: "center", padding: 12, backgroundColor: "#071327", borderBottomWidth: 1, borderBottomColor: "#0f2040" },
  modalTitle: { flex: 1, fontSize: 18, fontWeight: "700", color: "#d6f4ff" },
  closeBtn: { color: "#27c9a5", fontSize: 16, fontWeight: "600" },
  searchRow: { padding: 12 },
  searchInput: { backgroundColor: "#112544", color: "#d6f4ff", borderRadius: 10, padding: 12, fontSize: 16, borderWidth: 1, borderColor: "#1e3a5f" },
});
